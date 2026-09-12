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
            let sorted = insert_sort_below_top(plan, terms);
            // ROWID-ORDER ELISION: a table b-tree scan emits rows in
            // rowid order by construction — an ORDER BY that resolves to
            // the scan's rowid-alias column (ASC) sorts NOTHING. Dropping
            // the Sort node lets the statement stream (prepare/step) with
            // ZERO materialization instead of sort-buffering the whole
            // table (`SELECT id, val FROM d ORDER BY id` over 100k rows
            // previously held the full result set live).
            try_elide_rowid_order_sort(sorted)
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

        // SINGLE-TABLE ROWID-IN-EXPRESSION REWRITE: `rowid`/`_rowid_`/`oid`
        // referenced INSIDE an expression (`SELECT rowid*2`, `max(rowid)`,
        // `WHERE rowid % 2 = 0`, `ORDER BY rowid % 3`) evaluates against
        // materialized rows that carry table columns only — the bare-column
        // fast paths know the ROWID_PROJ sentinel, the expression evaluator
        // does not, so every such reference read NULL. For a single-table
        // query over a table WITH a rowid-alias column, rewriting the
        // spelling to the alias column name makes the value reachable from
        // every evaluation path at once (name lookups, compiled predicates,
        // aggregates, sorts). Real columns named rowid/_rowid_/oid shadow
        // the pseudo-column (SQLite rule) and are left alone; subquery
        // bodies are scope boundaries and are not descended into (their
        // own plan_select pass rewrites with their own FROM table);
        // joins/multi-table bodies are skipped entirely (bare rowid is
        // ambiguous there). No-alias tables keep the documented gap (the
        // rowid has no in-row value to rewrite to).
        let mut plan = plan;
        if let SelectBody::Simple(s) = &stmt.body {
            match &s.from {
                Some(TableExpression::Table {
                    name,
                    schema: None,
                    alias,
                    ..
                }) => {
                    if let Some(table) = self.catalog.get_table(name) {
                        rewrite_rowid_refs_in_plan(&mut plan, &table, alias.as_deref());
                    }
                }
                // MULTI-SIDE (join) FROM: every qualified rowid ref
                // (`u.rowid = o.u_id`) rewrites through the side it
                // names — the single-table hook above never fires for
                // these shapes, so the refs previously evaluated NULL
                // (alias tables) or mis-bound (slot tables).
                Some(from) => {
                    let mut names: Vec<(String, Option<String>)> = Vec::new();
                    collect_from_sides(from, &mut names);
                    let sides: Vec<(std::sync::Arc<Table>, Option<String>)> = names
                        .into_iter()
                        .filter_map(|(n, a)| self.catalog.get_table(&n).map(|t| (t, a)))
                        .collect();
                    if sides.len() > 1 {
                        rewrite_rowid_refs_in_plan_multi(&mut plan, &sides);
                    }
                }
                None => {}
            }
        }

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
                // 3. Collation attachment (SQLite): the ORDER BY term's
                //    own collation — an explicit COLLATE on the term
                //    (checked on the ORIGINAL term: the aggregate
                //    rewrite above replaces the term with a group-key
                //    column reference and would otherwise lose it), else
                //    a bare-column term's DECLARED collation. Attached
                //    AFTER alias / aggregate resolution so terms that
                //    resolved to an expression keep the expression's own
                //    semantics; every sort comparator (serial exec_sort,
                //    top-N, and the parallel path's Collate declination)
                //    honors the wrapper.
                if !matches!(&expr, Expr::Collate { .. }) {
                    let term_coll = if let Expr::Collate { collation, .. } = &t.expr {
                        Some(collation.clone())
                    } else if let Expr::Column { .. } = &expr {
                        s.from.as_ref().and_then(|from| {
                            let scope = collect_collation_scope(self.catalog, from);
                            column_declared_collation(self.catalog, &expr, &scope)
                        })
                    } else {
                        None
                    };
                    if let Some(coll) =
                        term_coll.filter(|c| !c.is_empty() && !c.eq_ignore_ascii_case("BINARY"))
                    {
                        expr = Expr::Collate {
                            expr: Box::new(expr),
                            collation: coll,
                        };
                    }
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

        // Inner-join reordering: flatten maximal INNER/CROSS join spines
        // (3+ relations), estimate each atom's cardinality from its pushed
        // access path + sqlite_stat1, and greedily rebuild the spine
        // smallest-filtered-first. Runs AFTER pushdown (estimates read the
        // pushed access paths) and BEFORE the INLJ rewrite (the reordered
        // tree is what INLJ converts).
        plan = reorder_inner_joins(self.catalog, plan);

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
        let mut collected_windows: Option<Vec<WindowExpr>> = None;
        if has_windows {
            let windows = self.collect_windows(&s.columns, &s.window)?;
            plan = Plan::Window {
                input: Box::new(plan),
                windows: windows.clone(),
            };
            collected_windows = Some(windows);
        }

        // Project FIRST, then DISTINCT. SQLite semantics: DISTINCT applies
        // to the projected columns, not the underlying row. If we put
        // Distinct before Project, the full row (including the rowid alias)
        // would be the dedup key, and every row would be unique.
        //
        // WINDOW columns: `exec_window` appends one column per window
        // expression (named `__win_N`) to its output. The projection must
        // reference those columns instead of re-evaluating the function
        // call as a plain scalar (unknown function -> NULL). Rewrite every
        // window call in the projected expressions to a column reference;
        // the output NAME stays the user alias or the pretty display.
        let project_columns: Vec<ProjectExpr> = s
            .columns
            .iter()
            .map(|c| self.result_column_to_project(c))
            .collect::<Result<_>>()?;
        let project_columns = match &collected_windows {
            Some(windows) if !windows.is_empty() => project_columns
                .into_iter()
                .map(|mut pe| {
                    pe.expr = rewrite_window_refs(&pe.expr, windows);
                    if pe.alias.is_none() {
                        // Bare window column: keep the pretty display name.
                        if let Expr::Column { name, .. } = &pe.expr {
                            if let Some(rest) = name.strip_prefix("__win_") {
                                if let Ok(idx) = rest.parse::<usize>() {
                                    pe.alias = Some(windows[idx].display_name.clone());
                                }
                            }
                        }
                    }
                    pe
                })
                .collect(),
            _ => project_columns,
        };
        plan = Plan::Project {
            input: Box::new(plan),
            columns: project_columns,
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
            TableExpression::Function { name, args, alias } => Ok(Plan::TableFunction {
                name: name.clone(),
                args: args.clone(),
                alias: alias.clone(),
            }),
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
                // ---- Single-table ON conjuncts push into the sides ----
                // For INNER/CROSS joins, ON and WHERE are semantically
                // interchangeable (SQLite's planner pushes single-table
                // ON conjuncts into the scans too): `ON a.y > 997 AND
                // a.y < b.y` becomes a filtered/indexed scan of a plus a
                // spanning join condition — instead of sweeping every
                // pair with a condition the nested loop re-evaluates per
                // pair. For OUTER joins only the NON-preserved side is
                // pushable, and only when that side has no unmatched-row
                // tail: LEFT keeps its right side pushable (a failing b
                // row can only ever null-extend), RIGHT its left side
                // (an unmatched a row emits nothing); FULL keeps nothing
                // (both sides feed tails).
                let (l, r, condition) = {
                    let push_left =
                        matches!(jt, JoinType::Inner | JoinType::Cross | JoinType::Right);
                    let push_right =
                        matches!(jt, JoinType::Inner | JoinType::Cross | JoinType::Left);
                    push_on_conjuncts_into_sides(
                        self.catalog,
                        l,
                        r,
                        condition,
                        push_left,
                        push_right,
                    )
                };
                // Hash whenever the condition carries at least one
                // column-to-column equality leaf (a single `l = r` OR an
                // AND-chain like `ON a.x = b.x AND a.y = b.y` — the
                // materialized hash join extracts every equi pair and
                // evaluates the rest as a residual per matched pair, which
                // strictly dominates the nested loop's O(n·m) condition
                // evaluation). Pure residual conditions (no equi leaf)
                // keep the nested loop.
                let has_equi = matches!(
                    constraint,
                    JoinConstraint::Natural | JoinConstraint::Using(_)
                ) || condition.as_ref().is_some_and(condition_has_equi_leaf);
                let algo = if has_equi {
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
        // A group term may carry an explicit COLLATE (`GROUP BY v COLLATE
        // NOCASE`) — the collation shapes grouping, not the term's VALUE:
        // a projection / ORDER BY reference to the bare `v` still means
        // this group key. Unwrap one Collate level for the match.
        let inner_display = match g {
            Expr::Collate { expr, .. } => format!("{:?}", expr),
            _ => g_display.clone(),
        };
        if g_display == e_display || inner_display == e_display {
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
                // json_group_object(k, v) carries the SAME synthesized
                // fragment as `collect_aggregates_rec` so the structural
                // match below succeeds.
                let synthesized: Option<Expr>;
                let call_arg: Option<&Expr> = if args.is_empty()
                    || args
                        .first()
                        .map(|a| matches!(a, Expr::Column { name, .. } if name == "*"))
                        .unwrap_or(false)
                {
                    None
                } else if (name.eq_ignore_ascii_case("json_group_object")
                    || name.eq_ignore_ascii_case("jsonb_group_object"))
                    && args.len() == 2
                {
                    let frag_name = if name.eq_ignore_ascii_case("jsonb_group_object") {
                        "__jsonb_object_frag"
                    } else {
                        "__json_object_frag"
                    };
                    synthesized = Some(Expr::Function {
                        name: frag_name.into(),
                        distinct: false,
                        args: args.clone(),
                        over: None,
                        filter: None,
                    });
                    synthesized.as_ref()
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
/// Replace every window-function call in `e` whose structural key matches
/// `windows[i].expr_key` with a column reference to `__win_{i}` (the
/// column `exec_window` appends). Recurses through all sub-expressions.
pub(crate) fn rewrite_window_refs(e: &Expr, windows: &[WindowExpr]) -> Expr {
    let key = format!("{:?}", e);
    if let Some(i) = windows.iter().position(|w| w.expr_key == key) {
        return Expr::Column {
            table: None,
            name: format!("__win_{i}"),
        };
    }
    // Recurse: rebuild the expression with rewritten children.
    match e {
        Expr::Binary { left, right, op } => Expr::Binary {
            left: Box::new(rewrite_window_refs(left, windows)),
            right: Box::new(rewrite_window_refs(right, windows)),
            op: *op,
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_window_refs(expr, windows)),
        },
        Expr::Function {
            name,
            distinct,
            args,
            over,
            filter,
        } => {
            // The whole call with `over` matched above (or not); rewrite
            // the ARGUMENTS of non-window calls only.
            if over.is_some() {
                e.clone()
            } else {
                Expr::Function {
                    name: name.clone(),
                    distinct: *distinct,
                    args: args
                        .iter()
                        .map(|a| rewrite_window_refs(a, windows))
                        .collect(),
                    over: None,
                    filter: filter.clone(),
                }
            }
        }
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_window_refs(expr, windows)),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_window_refs(expr, windows)),
            collation: collation.clone(),
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|b| Box::new(rewrite_window_refs(b, windows))),
            whens: whens
                .iter()
                .map(|(c, v)| {
                    (
                        rewrite_window_refs(c, windows),
                        rewrite_window_refs(v, windows),
                    )
                })
                .collect(),
            else_: else_
                .as_ref()
                .map(|x| Box::new(rewrite_window_refs(x, windows))),
        },
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|x| rewrite_window_refs(x, windows))
                .collect(),
        ),
        _ => e.clone(),
    }
}

pub fn expr_has_window(e: &Expr) -> bool {
    match e {
        Expr::Function { over: Some(_), .. } => true,
        // A window call nested in a scalar function's arguments
        // (`round(percent_rank() OVER (...), 4)`) still needs the Window
        // operator — recurse through the arguments (and FILTER).
        Expr::Function { args, filter, .. } => {
            args.iter().any(expr_has_window)
                || filter.as_ref().map(|f| expr_has_window(f)).unwrap_or(false)
        }
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
        "count" | "sum" | "avg" | "total" | "group_concat" | "string_agg" => true,
        "json_group_array" => n_args == 1,
        "json_group_object" => n_args == 2,
        "jsonb_group_array" => n_args == 1,
        "jsonb_group_object" => n_args == 2,
        "min" | "max" => n_args <= 1,
        // Percentile family (Turso percentile-extension parity).
        "stddev" | "stddev_samp" | "stddev_pop" | "median" => n_args == 1,
        "percentile_cont" | "percentile_disc" => n_args == 2,
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
                let fname = name.to_ascii_lowercase();
                let arg = if args.is_empty()
                    || (args.len() == 1
                        && matches!(&args[0], Expr::Column { name, .. } if name == "*"))
                {
                    None
                } else if (fname == "group_concat" || fname == "string_agg") && args.len() == 2 {
                    // group_concat(x, sep): the separator must be constant
                    // (SQLite's own constraint in windowed form; the
                    // non-windowed form evaluates it once per group —
                    // literal separators cover the practical universe).
                    let sep = match &args[1] {
                        Expr::Literal(Value::Text(t)) => Some(t.to_string()),
                        Expr::Literal(Value::Integer(i)) => Some(i.to_string()),
                        Expr::Literal(Value::Real(r)) => Some(r.to_string()),
                        _ => None,
                    };
                    out.push(AggExpr {
                        func: fname,
                        arg: Some(args[0].clone()),
                        sep,
                        distinct: *distinct,
                        alias: alias.clone(),
                        display_name: aggregate_display_name(name, *distinct, args),
                    });
                    return;
                } else if (fname == "percentile_cont" || fname == "percentile_disc")
                    && args.len() == 2
                {
                    // percentile_*(y, P): P must be a constant literal in
                    // [0, 100] (SQLite's percentile extension errors on
                    // non-constant or out-of-range P).
                    let pct = match &args[1] {
                        Expr::Literal(Value::Integer(i)) => Some(i.to_string()),
                        Expr::Literal(Value::Real(r)) => Some(r.to_string()),
                        _ => None,
                    };
                    let pct = match pct {
                        Some(s) => {
                            let v = s
                                .parse::<f64>()
                                .map_err(|_| {
                                    crate::error::Error::semantic(format!(
                                        "2nd argument to {fname} must be a number"
                                    ))
                                })
                                .ok();
                            match v {
                                Some(p) if (0.0..=100.0).contains(&p) => Some(s),
                                Some(_) => {
                                    // Out of range: SQLite errors. Emit the
                                    // aggregate anyway with an invalid P —
                                    // finalize returns NULL; the error path
                                    // stays simple.
                                    Some(s)
                                }
                                None => None,
                            }
                        }
                        None => None,
                    };
                    out.push(AggExpr {
                        func: fname,
                        arg: Some(args[0].clone()),
                        sep: pct,
                        distinct: *distinct,
                        alias: alias.clone(),
                        display_name: aggregate_display_name(name, *distinct, args),
                    });
                    return;
                } else if (fname == "json_group_object" || fname == "jsonb_group_object")
                    && args.len() == 2
                {
                    // Two-argument aggregate: accumulate the per-row
                    // fragment (a hidden scalar that the accumulator
                    // joins); finalize wraps in {} / an object container.
                    let frag_name = if fname == "jsonb_group_object" {
                        "__jsonb_object_frag"
                    } else {
                        "__json_object_frag"
                    };
                    Some(Expr::Function {
                        name: frag_name.into(),
                        distinct: false,
                        args: args.clone(),
                        over: None,
                        filter: None,
                    })
                } else {
                    // MAX(x) / MIN(x) / SUM(x): the single argument is the
                    // aggregate input; multi-arg calls (e.g. MAX(1,5,3))
                    // also use the first argument.
                    Some(args[0].clone())
                };
                out.push(AggExpr {
                    func: fname,
                    arg,
                    sep: None,
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
                for e in l.iter() {
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
                    Some(args[0].clone())
                };
                let extra_args: Vec<Expr> = if args.len() > 1 {
                    args[1..].to_vec()
                } else {
                    Vec::new()
                };
                out.push(WindowExpr {
                    func: name.to_ascii_lowercase(),
                    arg,
                    extra_args,
                    distinct: *distinct,
                    partition_by,
                    order_by,
                    frame,
                    alias: alias.clone(),
                    display_name: aggregate_display_name(name, *distinct, args),
                    expr_key: format!("{:?}", e),
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
        // Column operands may carry a COLLATE wrapper from collation
        // resolution (declared column collations / explicit COLLATE) —
        // see-through it for the column name; the VALUE side is returned
        // verbatim. Index-selection callers gate on the conjunct's own
        // collation (index_column_serves_conjunct) before using the pair.
        // Try left = literal
        if let Some(name) = column_name_of(left.as_ref()) {
            // Right side must not be a column ref (we want literal/param/value).
            if column_name_of(right.as_ref()).is_none() {
                return Some((name.to_string(), *right.clone()));
            }
        }
        // Try right = literal
        if let Some(name) = column_name_of(right.as_ref()) {
            if column_name_of(left.as_ref()).is_none() {
                return Some((name.to_string(), *left.clone()));
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
/// Compound-index candidate for `apply_where_for_scan`: the index whose
/// prefix is equality-bound by the most conjuncts, the bound key
/// expressions (in index column order), and the bound column names (for
/// residual computation). With `sqlite_stat1` statistics present,
/// `est_rows` carries SQLite's row-estimate model for the bound prefix
/// (`rows / (D1 × … × Dk)`) and drives the choice — the covered-prefix
/// heuristic only breaks ties between equal estimates.
struct IdxCandidate {
    index: std::sync::Arc<crate::schema::Index>,
    key_exprs: Vec<Expr>,
    bound_cols: Vec<String>,
    covered: usize,
    /// Estimated matching rows; `None` when no statistics exist for the
    /// index (the pre-ANALYZE behavior: rank by covered prefix only).
    est_rows: Option<i64>,
}

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
            }
        }
        // Compound-aware index selection: among all (equality conjunct,
        // index) pairs where the index's FIRST column is equality-bound,
        // pick the index whose PREFIX is bound by the MOST conjuncts —
        // `WHERE d = ? AND a = ?` then seeks the compound index
        // ida(d, a) with a two-column key instead of probing idd(d) and
        // filtering ~100 matches per hit. Single-column indexes win only
        // when no compound prefix covers more. (The executor's
        // IndexLookup already builds multi-column order keys.)
        let mut best: Option<IdxCandidate> = None;
        for conjunct in &conjuncts {
            let Some((col_name, value_expr)) = extract_eq_predicate(conjunct) else {
                continue;
            };
            for index in catalog.indexes_on_table(&table.name) {
                let Some(first_col) = index.columns.first() else {
                    continue;
                };
                if !first_col.name.eq_ignore_ascii_case(&col_name) {
                    continue;
                }
                // Collation gate: the index's first column must carry the
                // comparison's collation (SQLite rule). A NOCASE index
                // can't seek a BINARY equality — folding the probe key
                // would match case variants the BINARY comparison
                // excludes; a BINARY index can't seek a NOCASE /
                // RTRIM / custom comparison either. Mismatch falls back
                // to the scan + Filter path below.
                if !index_column_serves_conjunct(conjunct, first_col) {
                    continue;
                }
                // Bind index prefix columns 1.. from OTHER equality
                // conjuncts, in INDEX column order.
                let mut key_exprs = vec![value_expr.clone()];
                let mut bound_cols = vec![first_col.name.to_ascii_lowercase()];
                for ci in 1..index.columns.len() {
                    let want = &index.columns[ci];
                    let hit = conjuncts.iter().find_map(|oc| {
                        if exprs_equal_conjunct(oc, conjunct) {
                            return None;
                        }
                        let (cn, ve) = extract_eq_predicate(oc)?;
                        // Prefix columns bind only same-collation
                        // conjuncts (same rule as the first column).
                        if !index_column_serves_conjunct(oc, want) {
                            return None;
                        }
                        cn.eq_ignore_ascii_case(&want.name).then_some(ve.clone())
                    });
                    match hit {
                        Some(ve) => {
                            key_exprs.push(ve);
                            bound_cols.push(want.name.to_ascii_lowercase());
                        }
                        None => break,
                    }
                }
                let covered = key_exprs.len();
                // Stat-based estimate for the bound prefix (None without
                // ANALYZE statistics).
                let est_rows = catalog
                    .index_stats(&index.name)
                    .map(|s| s.estimate_eq(covered));
                // Ranking: estimated rows when BOTH sides carry stats
                // (ties fall to the covered-prefix heuristic); the
                // covered-prefix heuristic whenever either side lacks
                // stats (the pre-ANALYZE behavior).
                let better = match (&best, est_rows) {
                    (None, _) => true,
                    (Some(c), Some(ne)) => match c.est_rows {
                        Some(ce) => ne < ce || (ne == ce && covered > c.covered),
                        None => covered > c.covered,
                    },
                    (Some(c), None) => covered > c.covered,
                };
                if better {
                    best = Some(IdxCandidate {
                        index: index.clone(),
                        key_exprs,
                        bound_cols,
                        covered,
                        est_rows,
                    });
                }
            }
        }
        if let Some(IdxCandidate {
            index,
            key_exprs,
            bound_cols,
            est_rows,
            ..
        }) = best
        {
            // Stat-based scan guard: an index whose bound prefix matches
            // (nearly) the whole table is a net LOSS — each hit costs an
            // index seek PLUS a row fetch, so a sequential scan filtering
            // in place wins. SQLite's cost model makes the same call. The
            // guard only fires with real statistics (ANALYZE'd); the
            // heuristic path keeps the index as before.
            let unselective = match est_rows {
                Some(est) => {
                    let n = catalog
                        .index_stats(&index.name)
                        .map(|s| s.rows)
                        .unwrap_or(0);
                    n > 0 && est * 4 > n * 3 // > 75% of the table matches
                }
                None => false,
            };
            if !unselective {
                // Residual: conjuncts NOT consumed as index keys (an
                // equality conjunct whose column is a bound prefix column was
                // consumed; everything else survives as a Filter).
                let other_conjuncts: Vec<Expr> = conjuncts
                    .iter()
                    .filter(|c| {
                        extract_eq_predicate(c)
                            .map(|(cn, _)| !bound_cols.contains(&cn.to_ascii_lowercase()))
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect();
                let lookup = Plan::IndexLookup {
                    table: table.clone(),
                    alias: alias.clone(),
                    index,
                    key_exprs,
                };
                if other_conjuncts.is_empty() {
                    return lookup;
                }
                return Plan::Filter {
                    input: Box::new(lookup),
                    predicate: combine_and(&other_conjuncts),
                };
            }
            // Unselective: fall through to the scan+Filter paths below.
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
            // Unqualified column reference, possibly COLLATE-wrapped by
            // collation resolution (declared column collation / explicit
            // COLLATE).
            let unqualified_col = |e: &Expr| -> Option<String> {
                let inner = match e {
                    Expr::Collate { expr, .. } => expr.as_ref(),
                    other => other,
                };
                if let Expr::Column { table: None, name } = inner {
                    Some(name.clone())
                } else {
                    None
                }
            };
            if let Some(name) = unqualified_col(expr.as_ref()) {
                for index in catalog.indexes_on_table(&table.name) {
                    // Single-column index whose (only) column matches: each
                    // list member becomes one equality seek. The IN's
                    // comparison collation must match the index column's
                    // (same rule as equality seeks).
                    if index.columns.len() == 1
                        && index.columns[0].name.eq_ignore_ascii_case(&name)
                        && index_column_serves_conjunct(conjunct, &index.columns[0])
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
    // Encoding gate: the in-memory index b-trees order TEXT keys by the
    // engine's order-preserving key encoding (code-point order), but on
    // non-UTF-8 files range comparisons follow the FILE's byte order
    // (SQLite's BINARY collation — see value_cmp_conn). The two orders
    // disagree for supplementary-plane text, so an index seek bounded in
    // code-point space would over- AND under-select relative to the
    // byte-order predicate. Decline the index: the range conjunct stays
    // a scan filter, evaluated with the encoding-aware comparator.
    // Equality lookups (IndexPoint / IndexNestedLoopJoin / IN) are
    // unaffected — both orders induce the same equality relation.
    if crate::executor::conn_enc_tag() != 1 {
        return None;
    }
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
            // BETWEEN (collation must match the index column's — same
            // gate as equality: a NOCASE index can't bound a BINARY range
            // and vice versa).
            if let Some((col, lo, hi)) = extract_between(conjunct) {
                if bare_col(&col) == first_name && index_column_serves_conjunct(conjunct, first_col)
                {
                    matched = true;
                    start = Some((lo, true));
                    end = Some((hi, true));
                    continue;
                }
            }
            // Range ops.
            if let Some((col, op, val)) = extract_range(conjunct) {
                if bare_col(&col) == first_name && index_column_serves_conjunct(conjunct, first_col)
                {
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
        if let Some(name) = column_name_of(expr.as_ref()) {
            return Some((name.to_string(), *low.clone(), *high.clone()));
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
        // `col OP value` (col possibly COLLATE-wrapped — see
        // extract_eq_predicate)
        if let (Some(col), rhs) = (column_name_of(left.as_ref()), right.as_ref()) {
            if column_name_of(rhs).is_none() {
                return Some((col.to_string(), op_str.to_string(), *right.clone()));
            }
        }
        // `value OP col` — flip the direction.
        if let (Some(col), lhs) = (column_name_of(right.as_ref()), left.as_ref()) {
            if column_name_of(lhs).is_none() {
                let flipped = match op_str {
                    "<" => ">",
                    "<=" => ">=",
                    ">" => "<",
                    ">=" => "<=",
                    _ => return None,
                };
                return Some((col.to_string(), flipped.to_string(), *left.clone()));
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
                for e in es.iter() {
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

/// Push an ON condition's single-table conjuncts into the join's side
/// plans (the WHERE side of the same machinery — `pushdown_filter` /
/// `apply_where_for_scan`, so index selection, rowid ranges and IN
/// lookups all light up for ON predicates too). The caller gates the
/// sides by join type (see the Join arm: INNER/CROSS push both, LEFT
/// pushes the non-preserved right, RIGHT the non-preserved left, FULL
/// neither). Ambiguous conjuncts (unqualified refs that bind BOTH
/// sides' same-named columns), subquery-carrying conjuncts and
/// unresolvable side shapes stay in the ON condition — correctness by
/// fallback. Returns the (left, right, condition) triple.
fn push_on_conjuncts_into_sides(
    catalog: &Catalog,
    left: Plan,
    right: Plan,
    condition: Option<Expr>,
    push_left: bool,
    push_right: bool,
) -> (Plan, Plan, Option<Expr>) {
    let Some(cond) = condition else {
        return (left, right, None);
    };
    // Subquery conjuncts keep the full combined-row scope.
    if crate::executor::expr_has_subquery(&cond) {
        return (left, right, Some(cond));
    }
    let l_cols = plan_column_refs(&left);
    let r_cols = plan_column_refs(&right);
    if l_cols.is_empty() || r_cols.is_empty() {
        return (left, right, Some(cond));
    }
    let conjuncts = split_and_chain(&cond);
    let mut left_preds: Vec<Expr> = Vec::new();
    let mut right_preds: Vec<Expr> = Vec::new();
    let mut kept: Vec<Expr> = Vec::new();
    for c in conjuncts {
        let bound_l = conjunct_bound_by(&c, &l_cols);
        let bound_r = conjunct_bound_by(&c, &r_cols);
        if bound_l && !bound_r && push_left {
            left_preds.push(c);
        } else if bound_r && !bound_l && push_right {
            right_preds.push(c);
        } else {
            kept.push(c);
        }
    }
    let new_left = if left_preds.is_empty() {
        left
    } else {
        pushdown_filter(catalog, left, &combine_and(&left_preds))
    };
    let new_right = if right_preds.is_empty() {
        right
    } else {
        pushdown_filter(catalog, right, &combine_and(&right_preds))
    };
    let new_cond = if kept.is_empty() {
        None
    } else {
        Some(combine_and(&kept))
    };
    (new_left, new_right, new_cond)
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

        // INNER/CROSS joins: a top conjunct that references BOTH sides is a
        // JOIN CONDITION in disguise (`FROM a, b WHERE a.x = b.x` is
        // exactly `… JOIN b ON a.x = b.x`) — fuse it into the join's
        // condition instead of leaving it as a post-filter over a cross
        // product. With the multi-equi Hash rule that turns the classic
        // implicit-join syntax into a hash join. Outer joins keep every
        // predicate on top (their ON/WHERE semantics differ).
        let (fusable, kept_top): (Vec<Expr>, Vec<Expr>) =
            if matches!(join_type, JoinType::Inner | JoinType::Cross) {
                top_preds.into_iter().partition(|c| {
                    !crate::executor::expr_has_subquery(c)
                        && conjunct_spans_both_sides(c, &left_cols, &right_cols)
                })
            } else {
                (Vec::new(), top_preds)
            };

        let condition = if fusable.is_empty() {
            condition.clone()
        } else {
            let mut parts: Vec<Expr> = condition.iter().cloned().collect();
            parts.extend(fusable);
            Some(combine_and(&parts))
        };
        // Recompute the algorithm over the (possibly extended) condition:
        // Hash whenever an equi leaf appeared; a residual-only condition
        // keeps the nested loop; an untouched Natural/None shape keeps its
        // original algorithm.
        let algorithm = match &condition {
            Some(c) if condition_has_equi_leaf(c) => JoinAlgorithm::Hash,
            Some(_) => JoinAlgorithm::NestedLoop,
            None => *algorithm,
        };

        let new_join = Plan::Join {
            left: Box::new(new_left),
            right: Box::new(new_right),
            join_type: *join_type,
            condition,
            algorithm,
        };

        if kept_top.is_empty() {
            new_join
        } else {
            // Wrap remaining (non-pushable, non-fusable) conjuncts in a
            // top-level Filter, but try the cheap index/rowid path on the
            // Join as a whole first.
            apply_where_for_scan(catalog, new_join, &combine_and(&kept_top))
        }
    } else {
        // Not a Join — let apply_where_for_scan handle the Scan + index path,
        // or wrap in a Filter for other plan shapes.
        apply_where_for_scan(catalog, plan, &combine_and(&conjuncts))
    }
}

/// True when the conjunct references at least one column of EACH side of
/// a join — the marker of a join condition living in a WHERE clause.
/// (Subquery-carrying conjuncts are excluded by the caller: their hidden
/// outer references need the full combined row in scope.)
fn conjunct_spans_both_sides(
    c: &Expr,
    left_cols: &[(Option<String>, String)],
    right_cols: &[(Option<String>, String)],
) -> bool {
    let refs = collect_column_refs(c);
    if refs.is_empty() {
        return false;
    }
    let ref_bound = |r: &(Option<String>, String), cols: &[(Option<String>, String)]| match &r.0 {
        Some(t) => cols.iter().any(|(p, n)| {
            p.as_ref()
                .map(|pv| pv.eq_ignore_ascii_case(t))
                .unwrap_or(false)
                && n.eq_ignore_ascii_case(&r.1)
        }),
        None => cols.iter().any(|(_, n)| n.eq_ignore_ascii_case(&r.1)),
    };
    refs.iter().any(|r| ref_bound(r, left_cols)) && refs.iter().any(|r| ref_bound(r, right_cols))
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

// ============================================================================
// Inner-join reordering (greedy, cardinality-driven)
// ============================================================================
//
// SQLite's planner SEARCHES join orders; a syntactic left-deep tree in
// FROM-clause order is whatever the user happened to write. For INNER/CROSS
// equi-join chains the join order decides the size of every intermediate
// result — the single biggest cost lever on multi-table queries:
// `big1 JOIN big2 ON … JOIN tiny ON … WHERE tiny.k = 5` computes the full
// big1×big2 intermediate before tiny's five rows ever filter it, when
// tiny-first would drive both bigs by index instead.
//
// `reorder_inner_joins` flattens maximal INNER/CROSS join spines of 3+
// relation atoms, estimates each atom's output cardinality from its
// (already pushed-down) access path plus `sqlite_stat1`, and greedily
// rebuilds the spine left-deep: the smallest atom seeds, then the smallest
// atom connected to the covered set by a pool conjunct joins next;
// disconnected picks only happen for true cartesian products. The original
// ON conditions — and the multi-relation WHERE conjuncts that pushdown
// left on top, which for INNER joins ARE join conditions — are re-attached
// to the first join node that covers their relations. Anything that can't
// be attached safely (subquery-carrying conjuncts, unresolvable
// references) stays in a Filter on top, exactly where it was.
//
// SEMANTICS: inner joins are commutative and associative — the multiset of
// combined rows is order-independent. Column ORDER is preserved: every
// pool conjunct's unqualified references are rewritten to their owning
// atom's (unique) name at flatten time, and the reordered spine is wrapped
// in a projection of bare column references that restores the original
// column order (the fused executor's synthesized-project pattern), so
// every consumer above the spine sees the same columns in the same order
// as the syntactic tree.
//
// Safety gates (any miss → the spine keeps its syntactic shape):
// - ≥ 3 atoms and ≤ REORDER_MAX_ATOMS. Two-atom spines are left alone:
//   the executor's hash join already picks its own build side and the
//   INLJ pass already tries both directions.
// - Every atom's output column names are statically computable AND unique
//   across the spine (visible names). Hidden rowid slots are exempt —
//   they are name-unresolvable by design.
// - Unqualified references in pool conjuncts must be unambiguous (exactly
//   one owning atom) — otherwise name resolution could follow the
//   reordering instead of the query.
// - Pool conjuncts carry no subqueries (their hidden outer references
//   must keep the full combined row in scope — the same rule pushdown
//   applies).
// - Outer-join boundaries PIN their subtrees: a LEFT/RIGHT/FULL join node
//   is an atom like any other (its own inner sub-spines still reorder
//   independently — the driver recurses into it), never re-associated
//   across its boundary.

/// SQLite's default table-size assumption when ANALYZE hasn't run
/// (nRowLogEst 200 ≈ 2^20 rows).
const UNANALYZED_ROWS: i64 = 1 << 20;

/// Spine size cap: the greedy enumeration is O(n² · conjuncts) at plan
/// time; beyond this the planning cost outweighs the reorder win.
/// (SQLite's own join-search loop is capped similarly.)
const REORDER_MAX_ATOMS: usize = 64;

/// Driver: walk the plan; at every node recurse first, then (at Filter
/// and spine-root positions) attempt a spine reorder.
pub(crate) fn reorder_inner_joins(catalog: &Catalog, plan: Plan) -> Plan {
    match plan {
        Plan::Join {
            left,
            right,
            join_type,
            condition,
            algorithm,
        } => {
            // Both children first: atoms and pinned subtrees get their own
            // driver pass (nested spines reorder independently).
            let node = walk_join_children(catalog, *left, *right, join_type, condition, algorithm);
            // Reached by the driver, this node is the ROOT of a maximal
            // inner spine (its parent is a non-inner-join or a wrapper).
            if matches!(join_type, JoinType::Inner | JoinType::Cross) {
                if let Some(reordered) = try_reorder_spine(catalog, &node, None) {
                    return reordered;
                }
            }
            node
        }
        Plan::Filter { input, predicate } => {
            // A Filter directly above an inner-join spine root: pushdown
            // already moved single-relation conjuncts into the atoms; the
            // rest are multi-relation predicates — join conditions in
            // disguise for INNER joins. Attempt the reorder WITH them in
            // the pool BEFORE any plain spine attempt: a successful plain
            // attempt would hide the spine behind a restoration Project
            // and the conjuncts could never attach.
            if let Plan::Join {
                left,
                right,
                join_type: JoinType::Inner | JoinType::Cross,
                condition,
                algorithm,
            } = *input
            {
                let node = walk_join_children(
                    catalog,
                    *left,
                    *right,
                    JoinType::Inner,
                    condition,
                    algorithm,
                );
                if let Some(reordered) = try_reorder_spine(catalog, &node, Some(&predicate)) {
                    return reordered;
                }
                return Plan::Filter {
                    input: Box::new(node),
                    predicate,
                };
            }
            let inner = reorder_inner_joins(catalog, *input);
            Plan::Filter {
                input: Box::new(inner),
                predicate,
            }
        }
        // Single-input wrappers: recurse via the driver (a spine may hide
        // under any of them).
        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(reorder_inner_joins(catalog, *input)),
            columns,
        },
        Plan::Sort { input, terms } => Plan::Sort {
            input: Box::new(reorder_inner_joins(catalog, *input)),
            terms,
        },
        Plan::Limit {
            input,
            count,
            offset,
        } => Plan::Limit {
            input: Box::new(reorder_inner_joins(catalog, *input)),
            count,
            offset,
        },
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => Plan::Aggregate {
            input: Box::new(reorder_inner_joins(catalog, *input)),
            group_by,
            aggregates,
        },
        Plan::Window { input, windows } => Plan::Window {
            input: Box::new(reorder_inner_joins(catalog, *input)),
            windows,
        },
        Plan::Distinct { input } => Plan::Distinct {
            input: Box::new(reorder_inner_joins(catalog, *input)),
        },
        Plan::Subquery { plan } => Plan::Subquery {
            plan: Box::new(reorder_inner_joins(catalog, *plan)),
        },
        Plan::Union { left, right, all } => Plan::Union {
            left: Box::new(reorder_inner_joins(catalog, *left)),
            right: Box::new(reorder_inner_joins(catalog, *right)),
            all,
        },
        Plan::Intersect { left, right } => Plan::Intersect {
            left: Box::new(reorder_inner_joins(catalog, *left)),
            right: Box::new(reorder_inner_joins(catalog, *right)),
        },
        Plan::Except { left, right } => Plan::Except {
            left: Box::new(reorder_inner_joins(catalog, *left)),
            right: Box::new(reorder_inner_joins(catalog, *right)),
        },
        Plan::IndexNestedLoopJoin {
            outer,
            inner_table,
            inner_alias,
            inner_index,
            outer_key_col,
        } => Plan::IndexNestedLoopJoin {
            outer: Box::new(reorder_inner_joins(catalog, *outer)),
            inner_table,
            inner_alias,
            inner_index,
            outer_key_col,
        },
        // Leaves and everything else pass through untouched.
        other => other,
    }
}

/// Shared prologue of both reorder attempts: walk a Join node's children
/// (Inner/Cross children continue the spine; anything else is an atom
/// whose internal plan gets the full driver pass) and rebuild the node.
fn walk_join_children(
    catalog: &Catalog,
    left: Plan,
    right: Plan,
    join_type: JoinType,
    condition: Option<Expr>,
    algorithm: JoinAlgorithm,
) -> Plan {
    Plan::Join {
        left: Box::new(reorder_spine_child(catalog, left)),
        right: Box::new(reorder_spine_child(catalog, right)),
        join_type,
        condition,
        algorithm,
    }
}

/// Walk INSIDE a spine (the driver committed to flattening it at the
/// root): Inner/Cross join children continue the spine — their
/// reassociation is the root's job, so no local reorder attempt — while
/// every other child is an ATOM whose internal plan gets the full driver
/// pass (nested spines inside pinned outer-join subtrees or subqueries
/// still reorder on their own).
fn reorder_spine_child(catalog: &Catalog, plan: Plan) -> Plan {
    match plan {
        Plan::Join {
            left,
            right,
            join_type: JoinType::Inner | JoinType::Cross,
            condition,
            algorithm,
        } => Plan::Join {
            left: Box::new(reorder_spine_child(catalog, *left)),
            right: Box::new(reorder_spine_child(catalog, *right)),
            join_type: JoinType::Inner,
            condition,
            algorithm,
        },
        other => reorder_inner_joins(catalog, other),
    }
}

/// The output column names an atom's executor reports — must mirror each
/// executor EXACTLY (qualified for Scan / IndexRange, plain for the
/// lookup variants, hidden rowid slots where the executor adds them, CTE
/// columns for CteRows, left+right concatenation for pinned join
/// subtrees). `None` for plans whose names aren't statically computable
/// (Subquery, TableFunction, Project-wrapped views) — the caller aborts
/// the reorder.
fn atom_out_cols(plan: &Plan) -> Option<Vec<String>> {
    let cols = match plan {
        Plan::Scan { table, alias, .. } => {
            let mut v = scan_qualified_cols(table, alias);
            if wants_rowid_slot(table) {
                // Mirrors the executor: side-qualified hidden slot
                // (`prefix.\0rowid`) — see hidden_rowid_slot.
                v.push(hidden_rowid_slot(alias.as_deref().unwrap_or(&table.name)));
            }
            v
        }
        Plan::RowidRange { table, .. } | Plan::IndexLookup { table, .. } => {
            let mut v: Vec<String> = table.col_names.iter().cloned().collect();
            if wants_rowid_slot(table) {
                // Mirrors the executors: alias-less drivers qualify the
                // hidden slot with the table name.
                v.push(hidden_rowid_slot(&table.name));
            }
            v
        }
        Plan::RowidLookup { table, .. }
        | Plan::RowidIn { table, .. }
        | Plan::IndexIn { table, .. } => table.col_names.iter().cloned().collect(),
        Plan::IndexRange { table, alias, .. } => scan_qualified_cols(table, alias),
        Plan::Filter { input, .. } | Plan::Subquery { plan: input } => return atom_out_cols(input),
        Plan::Join { left, right, .. } => {
            let mut v = atom_out_cols(left)?;
            v.extend(atom_out_cols(right)?);
            v
        }
        Plan::CteRows { columns, .. } => columns.iter().cloned().collect(),
        _ => return None,
    };
    Some(cols)
}

/// Qualified scan column names ("prefix.col"), mirroring
/// `scan_output_columns` without the hidden slot (the caller appends it
/// itself where the executor does).
fn scan_qualified_cols(table: &Arc<Table>, alias: &Option<String>) -> Vec<String> {
    let prefix = alias.as_deref().unwrap_or(&table.name);
    if prefix == table.name {
        table.qualified_col_names.iter().cloned().collect()
    } else {
        table
            .columns
            .iter()
            .map(|c| format!("{}.{}", prefix, c.name))
            .collect()
    }
}

/// Table row-count hint from `sqlite_stat1` (any index of the table
/// carries the table's row count), else SQLite's unanalyzed default.
fn table_rows_hint(catalog: &Catalog, table: &Table) -> i64 {
    catalog
        .indexes_on_table(&table.name)
        .iter()
        .find_map(|idx| catalog.index_stats(&idx.name))
        .and_then(|s| if s.rows > 0 { Some(s.rows) } else { None })
        .unwrap_or(UNANALYZED_ROWS)
}

/// Estimated OUTPUT rows of an atom (planning-time only: stat1 + the
/// pushed access path; never a b-tree walk — plans are cached).
fn atom_est_rows(catalog: &Catalog, plan: &Plan) -> i64 {
    match plan {
        Plan::RowidLookup { .. } => 1,
        Plan::RowidIn { values, .. } => (values.len() as i64).max(1),
        Plan::RowidRange {
            table, start, end, ..
        } => {
            // Literal integer bounds: the exact span (clamped to the table
            // hint); parameterized bounds: SQLite's rows/4 range guess.
            let lit = |e: &Option<Expr>| match e.as_ref() {
                Some(Expr::Literal(Value::Integer(i))) => Some(*i),
                _ => None,
            };
            match (lit(start), lit(end)) {
                (Some(s), Some(e)) if e >= s => (e - s + 1).min(table_rows_hint(catalog, table)),
                _ => (table_rows_hint(catalog, table) / 4).max(1),
            }
        }
        Plan::IndexLookup {
            index, key_exprs, ..
        } => {
            let k = key_exprs.len();
            catalog
                .index_stats(&index.name)
                .map(|s| s.estimate_eq(k))
                .filter(|&e| e > 0)
                .unwrap_or(10)
        }
        Plan::IndexIn {
            index, key_exprs, ..
        } => {
            // key_exprs is the IN-list itself (single-column index IN);
            // the member count scales the lookup fan-out.
            let members = key_exprs.len() as i64;
            let base = catalog
                .index_stats(&index.name)
                .map(|s| s.estimate_eq(1))
                .filter(|&e| e > 0)
                .unwrap_or(10);
            base.saturating_mul(members).max(1)
        }
        Plan::IndexRange { index, .. } => catalog
            .index_stats(&index.name)
            .map(|s| s.rows / 4)
            .filter(|&e| e > 0)
            .unwrap_or(UNANALYZED_ROWS / 4),
        Plan::Filter { input, predicate } => {
            let base = atom_est_rows(catalog, input);
            filter_output_est(base, predicate)
        }
        Plan::Scan { table, .. } => table_rows_hint(catalog, table),
        Plan::CteRows { rows, .. } => (rows.len() as i64).max(1),
        // Pinned outer-join subtrees and anything exotic: unknown (the
        // unanalyzed default — big enough to lose every size contest).
        _ => UNANALYZED_ROWS,
    }
}

/// SQLite-style blind selectivity: each AND conjunct shrinks the output
/// (equity 1/10, range 1/3, IN-list k/10 capped at 1, unknown 1/2).
fn filter_output_est(rows: i64, predicate: &Expr) -> i64 {
    let mut est = rows as f64;
    for c in split_and_chain(predicate) {
        est *= conjunct_selectivity(&c);
    }
    est.max(1.0) as i64
}

fn conjunct_selectivity(c: &Expr) -> f64 {
    match c {
        Expr::Binary {
            op: BinaryOp::Eq, ..
        } => 0.1,
        Expr::Binary {
            op:
                op @ (BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq | BinaryOp::NotEq),
            ..
        } => {
            let _ = op;
            1.0 / 3.0
        }
        Expr::Between { .. } => 1.0 / 3.0,
        Expr::In {
            source: crate::sql::ast::InSource::List(es),
            ..
        } => (es.len() as f64 / 10.0).min(1.0),
        Expr::Like { .. } => 0.25,
        _ => 0.5,
    }
}

/// One pooled join conjunct: the expression (unqualified refs rewritten
/// to owning-atom names) and the atom set it references.
struct PooledConjunct {
    expr: Expr,
    /// Atom indices the conjunct's references resolve to.
    rels: Vec<usize>,
}

/// One flattened spine condition: an AND-leaf plus the atom window of
/// the join node it came from (lo..hi) and its split point (mid: the
/// left subtree covered lo..mid, the right mid..hi).
struct SpineCond {
    leaf: Expr,
    lo: usize,
    mid: usize,
    hi: usize,
}

/// Flatten the spine (left-to-right atom order, matching the executor's
/// combined-column order) while recording each ON condition's original
/// atom window — the evaluation context its references resolved against.
fn flatten_collect(
    plan: &Plan,
    base: usize,
    atoms: &mut Vec<Plan>,
    conds: &mut Vec<SpineCond>,
) -> usize {
    if let Plan::Join {
        left,
        right,
        join_type: JoinType::Inner | JoinType::Cross,
        condition,
        ..
    } = plan
    {
        let ln = flatten_collect(left, base, atoms, conds);
        let rn = flatten_collect(right, base + ln, atoms, conds);
        if let Some(c) = condition {
            for leaf in split_and_chain(c) {
                conds.push(SpineCond {
                    leaf,
                    lo: base,
                    mid: base + ln,
                    hi: base + ln + rn,
                });
            }
        }
        ln + rn
    } else {
        atoms.push(plan.clone());
        1
    }
}

/// Resolve a column reference against atoms [lo, hi) — mirroring
/// `resolve_column_index`'s pass order (qualified-exact over the whole
/// window, then exact, then suffix; first match wins) — returning the
/// owning atom and its canonical column name. Hidden rowid slots never
/// match (same as the executor's resolver).
fn resolve_ref_window(
    table: &Option<String>,
    name: &str,
    lo: usize,
    hi: usize,
    atom_cols: &[Vec<String>],
) -> Option<(usize, String)> {
    if let Some(t) = table {
        // Qualified reference: dotted-name exact match, then PLAIN-name
        // exact match (the lookup-family atoms report unqualified
        // columns). NO suffix fallback — `small.k` must never bind to a
        // same-named `big1.k`: this is SIDE-SCOPED binding, not the
        // combined-list resolution of the general evaluator, and a wrong
        // window binding would silently rewrite the join key.
        for (i, cols) in atom_cols.get(lo..hi).unwrap_or(&[]).iter().enumerate() {
            let i = i + lo;
            for c in cols {
                if is_hidden_rowid(c.as_str()) {
                    continue;
                }
                if let Some(pos) = c.rfind('.') {
                    if c[..pos].eq_ignore_ascii_case(t) && c[pos + 1..].eq_ignore_ascii_case(name) {
                        return Some((i, c.clone()));
                    }
                }
            }
        }
        for (i, cols) in atom_cols.get(lo..hi).unwrap_or(&[]).iter().enumerate() {
            let i = i + lo;
            for c in cols {
                if is_hidden_rowid(c.as_str()) {
                    continue;
                }
                if c.eq_ignore_ascii_case(name) {
                    return Some((i, c.clone()));
                }
            }
        }
        return None;
    }
    for (i, cols) in atom_cols.get(lo..hi).unwrap_or(&[]).iter().enumerate() {
        let i = i + lo;
        for c in cols {
            if is_hidden_rowid(c.as_str()) {
                continue;
            }
            if c.eq_ignore_ascii_case(name) {
                return Some((i, c.clone()));
            }
        }
    }
    for (i, cols) in atom_cols.get(lo..hi).unwrap_or(&[]).iter().enumerate() {
        let i = i + lo;
        for c in cols {
            if is_hidden_rowid(c.as_str()) {
                continue;
            }
            if let Some(pos) = c.rfind('.') {
                if c[pos + 1..].eq_ignore_ascii_case(name) {
                    return Some((i, c.clone()));
                }
            }
        }
    }
    None
}

/// Rewrite every `Expr::Column` reference in `e` to its owning atom's
/// canonical name (qualified refs included — the canonical form makes
/// resolution order-independent), collecting the atoms referenced.
/// Returns false when any reference fails to resolve in the window.
/// `atom_names[i]` carries atom i's (table name, alias) pair for the
/// qualifier-preservation rule on plain canonical names.
fn rewrite_all_refs(
    e: &mut Expr,
    lo: usize,
    hi: usize,
    atom_cols: &[Vec<String>],
    atom_names: &[(String, Option<String>)],
    rels: &mut Vec<usize>,
) -> bool {
    match e {
        Expr::Column { table, name } => match resolve_ref_window(table, name, lo, hi, atom_cols) {
            Some((atom, canon)) => {
                // Keep the QUALIFIED form (Some("alias"), "col") when the
                // canonical name is dotted: the INLJ pass's key extraction
                // and every qualified-resolution path expect a real
                // qualifier, not a dotted unqualified name. For a PLAIN
                // canonical (lookup-family atoms report unqualified
                // columns) keep the original qualifier when it names the
                // owning atom — qualified references still resolve on
                // plain-named outputs through the exact-match fallback,
                // and the qualifier is what the INLJ key extraction needs.
                let (q, n) = canonical_ref(&canon, table, atom, atom_names);
                *table = q;
                *name = n;
                if !rels.contains(&atom) {
                    rels.push(atom);
                }
                true
            }
            None => false,
        },
        Expr::Collate { expr, .. } => rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels),
        Expr::Binary { left, right, .. } => {
            rewrite_all_refs(left, lo, hi, atom_cols, atom_names, rels)
                & rewrite_all_refs(right, lo, hi, atom_cols, atom_names, rels)
        }
        Expr::Unary { expr, .. } => rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels),
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels)
                & rewrite_all_refs(low, lo, hi, atom_cols, atom_names, rels)
                & rewrite_all_refs(high, lo, hi, atom_cols, atom_names, rels)
        }
        Expr::In { expr, source, .. } => {
            let mut ok = rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels);
            if let crate::sql::ast::InSource::List(es) = &*source {
                let mut new_members = Vec::with_capacity(es.len());
                for m in es.iter() {
                    let mut mc = m.clone();
                    ok &= rewrite_all_refs(&mut mc, lo, hi, atom_cols, atom_names, rels);
                    new_members.push(mc);
                }
                *source = crate::sql::ast::InSource::List(std::sync::Arc::new(new_members));
            }
            ok
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut ok = rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels)
                & rewrite_all_refs(pattern, lo, hi, atom_cols, atom_names, rels);
            if let Some(x) = escape {
                ok &= rewrite_all_refs(x, lo, hi, atom_cols, atom_names, rels);
            }
            ok
        }
        Expr::IsNull { expr, .. } => rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels),
        Expr::Is { left, right, .. } => {
            rewrite_all_refs(left, lo, hi, atom_cols, atom_names, rels)
                & rewrite_all_refs(right, lo, hi, atom_cols, atom_names, rels)
        }
        Expr::Function { args, filter, .. } => {
            let mut ok = true;
            for a in args {
                ok &= rewrite_all_refs(a, lo, hi, atom_cols, atom_names, rels);
            }
            if let Some(f) = filter {
                ok &= rewrite_all_refs(f, lo, hi, atom_cols, atom_names, rels);
            }
            ok
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            let mut ok = true;
            if let Some(o) = operand {
                ok &= rewrite_all_refs(o, lo, hi, atom_cols, atom_names, rels);
            }
            for (w, t) in whens {
                ok &= rewrite_all_refs(w, lo, hi, atom_cols, atom_names, rels);
                ok &= rewrite_all_refs(t, lo, hi, atom_cols, atom_names, rels);
            }
            if let Some(x) = else_ {
                ok &= rewrite_all_refs(x, lo, hi, atom_cols, atom_names, rels);
            }
            ok
        }
        Expr::Row(es) => {
            let mut ok = true;
            for x in es {
                ok &= rewrite_all_refs(x, lo, hi, atom_cols, atom_names, rels);
            }
            ok
        }
        Expr::Cast { expr, .. } => rewrite_all_refs(expr, lo, hi, atom_cols, atom_names, rels),
        Expr::Raise {
            message: Some(m), ..
        } => rewrite_all_refs(m, lo, hi, atom_cols, atom_names, rels),
        Expr::Raise { message: None, .. } => true,
        // Literal / Parameter / Subquery / Exists: pool conjuncts are
        // subquery-free by the caller's guard; literals carry no refs.
        _ => true,
    }
}

/// Bind one spine ON-condition leaf against its ORIGINAL node window:
/// an `l = r` leaf with column operands on both sides binds POSITIONALLY
/// (left operand against the node's left subtree, right against the
/// right — replicating the executor's `collect_eq_pairs`, including the
/// swapped retry). This is what makes `JOIN … USING (id)`-style
/// unqualified `id = id` leaves survive reassociation: each operand
/// rewrites to ITS side's column instead of both falling to the same
/// first-match name. Everything else resolves against the whole window.
fn bind_leaf(
    leaf: &Expr,
    sc: &SpineCond,
    atom_cols: &[Vec<String>],
    atom_names: &[(String, Option<String>)],
) -> PooledConjunct {
    if let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = leaf
    {
        if column_name_of(left).is_some() && column_name_of(right).is_some() {
            // Positional try 1: l in left window, r in right window.
            let l_ref = column_ref_of(left);
            let r_ref = column_ref_of(right);
            let try_bind = |l_lo: usize, l_hi: usize, r_lo: usize, r_hi: usize| match (
                resolve_ref_window(&l_ref.0, &l_ref.1, l_lo, l_hi, atom_cols),
                resolve_ref_window(&r_ref.0, &r_ref.1, r_lo, r_hi, atom_cols),
            ) {
                (Some((la, ln)), Some((ra, rn))) => Some(((la, ln), (ra, rn))),
                _ => None,
            };
            let bound = try_bind(sc.lo, sc.mid, sc.mid, sc.hi)
                .or_else(|| try_bind(sc.mid, sc.hi, sc.lo, sc.mid));
            if let Some(((la, ln), (ra, rn))) = bound {
                let mut expr = leaf.clone();
                if let Expr::Binary { left, right, .. } = &mut expr {
                    rewrite_column_operand(left, &ln, la, atom_names);
                    rewrite_column_operand(right, &rn, ra, atom_names);
                }
                return PooledConjunct {
                    expr,
                    rels: vec![la, ra],
                };
            }
        }
    }
    // General path: whole-window resolution, refs rewritten to canonical
    // names. Unresolvable references (e.g. `rowid` pseudo-refs) leave the
    // rels empty — the caller keeps such conjuncts at the top.
    let mut expr = leaf.clone();
    let mut rels = Vec::new();
    if !rewrite_all_refs(&mut expr, sc.lo, sc.hi, atom_cols, atom_names, &mut rels) {
        rels.clear();
        return PooledConjunct {
            expr: leaf.clone(),
            rels,
        };
    }
    PooledConjunct { expr, rels }
}

/// Extract (table, name) from a column operand, seeing through COLLATE.
fn column_ref_of(e: &Expr) -> (Option<String>, String) {
    match e {
        Expr::Column { table, name } => (table.clone(), name.clone()),
        Expr::Collate { expr, .. } => column_ref_of(expr),
        _ => (None, String::new()),
    }
}

/// The qualified-reference form for a canonical column name:
/// "alias.col" → (Some("alias"), "col"). A plain canonical name (the
/// lookup-family atoms report unqualified columns) keeps the reference's
/// ORIGINAL qualifier when it names the owning atom (its table name or
/// alias) — that qualifier still resolves on plain-named outputs via the
/// exact-match fallback and is what the INLJ key extraction matches on.
fn canonical_ref(
    canon: &str,
    orig: &Option<String>,
    owner: usize,
    atom_names: &[(String, Option<String>)],
) -> (Option<String>, String) {
    if let Some(pos) = canon.rfind('.') {
        return (Some(canon[..pos].to_string()), canon[pos + 1..].to_string());
    }
    let keep = match (orig, atom_names.get(owner)) {
        (Some(t), Some((table_name, alias))) => {
            t.eq_ignore_ascii_case(table_name)
                || alias
                    .as_ref()
                    .map(|a| t.eq_ignore_ascii_case(a))
                    .unwrap_or(false)
        }
        _ => false,
    };
    if keep {
        (orig.clone(), canon.to_string())
    } else {
        (None, canon.to_string())
    }
}

/// An atom's (table name, alias) pair — the names a qualifier may refer
/// to it by. Lookup-family atoms report plain columns, so the alias
/// information lives here rather than in the column names themselves.
fn atom_name_pair(plan: &Plan) -> (String, Option<String>) {
    let unknown = (String::new(), None);
    match plan {
        Plan::Scan { table, alias, .. }
        | Plan::RowidLookup { table, alias, .. }
        | Plan::RowidIn { table, alias, .. }
        | Plan::RowidRange { table, alias, .. }
        | Plan::IndexIn { table, alias, .. }
        | Plan::IndexLookup { table, alias, .. }
        | Plan::IndexRange { table, alias, .. } => (table.name.clone(), alias.clone()),
        Plan::Filter { input, .. } => atom_name_pair(input),
        _ => unknown,
    }
}

/// Rewrite the column operand in place (through COLLATE wrappers) to
/// its canonical, qualifier-preserving reference form.
fn rewrite_column_operand(
    op: &mut Expr,
    canonical: &str,
    owner: usize,
    atom_names: &[(String, Option<String>)],
) {
    match op {
        Expr::Column { table, name } => {
            let (q, n) = canonical_ref(canonical, table, owner, atom_names);
            *table = q;
            *name = n;
        }
        Expr::Collate { expr, .. } => rewrite_column_operand(expr, canonical, owner, atom_names),
        _ => {}
    }
}

/// True when the condition contains at least one column-to-column
/// equality leaf (through COLLATE wrappers) — the marker for the Hash
/// join algorithm.
fn condition_has_equi_leaf(cond: &Expr) -> bool {
    match cond {
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => condition_has_equi_leaf(left) || condition_has_equi_leaf(right),
        Expr::Binary {
            op: BinaryOp::Eq,
            left,
            right,
        } => column_name_of(left).is_some() && column_name_of(right).is_some(),
        _ => false,
    }
}

/// Attempt one spine reorder. Returns None (caller keeps the syntactic
/// tree) whenever a safety gate trips or the greedy outcome is the
/// syntactic order itself with nothing new pooled from the filter.
fn try_reorder_spine(catalog: &Catalog, spine: &Plan, top_filter: Option<&Expr>) -> Option<Plan> {
    let dbg = std::env::var_os("RSQL_DBG_REORDER").is_some();
    // ---- Flatten (atoms + conditions with their original windows) ------
    let mut atom_plans: Vec<Plan> = Vec::new();
    let mut spine_conds: Vec<SpineCond> = Vec::new();
    flatten_collect(spine, 0, &mut atom_plans, &mut spine_conds);
    let n = atom_plans.len();
    if dbg {
        eprintln!(
            "[reorder] spine: {n} atoms, filter={}",
            top_filter.is_some()
        );
    }
    if !(3..=REORDER_MAX_ATOMS).contains(&n) {
        if dbg {
            eprintln!("[reorder] bail: atom count {n}");
        }
        return None;
    }

    // ---- Static output names + uniqueness gate --------------------------
    let mut atom_cols: Vec<Vec<String>> = Vec::with_capacity(n);
    for a in &atom_plans {
        atom_cols.push(atom_out_cols(a)?);
    }
    // (table name, alias) per atom — the names a qualifier may refer to
    // it by (lookup-family atoms report plain column names).
    let atom_names: Vec<(String, Option<String>)> = atom_plans.iter().map(atom_name_pair).collect();
    {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for cols in &atom_cols {
            for c in cols {
                if is_hidden_rowid(c) {
                    continue; // name-unresolvable by design
                }
                if !seen.insert(c.to_ascii_lowercase()) {
                    return None; // duplicate visible name: restoration by
                                 // name is unsafe — keep the syntactic tree
                }
            }
        }
    }

    // ---- Pool: spine conditions (bound to their original windows) ------
    let mut pool: Vec<PooledConjunct> = spine_conds
        .iter()
        .map(|sc| bind_leaf(&sc.leaf, sc, &atom_cols, &atom_names))
        .collect();
    // Unresolvable spine conditions (empty rels) never attach — evaluate
    // them once over the final combined row instead, exactly like a top
    // Filter. (Their references resolve — or fail — identically there:
    // same names, same first-match order.)
    let mut top_conjuncts: Vec<Expr> = pool
        .iter()
        .filter(|pc| pc.rels.is_empty())
        .map(|pc| pc.expr.clone())
        .collect();
    pool.retain(|pc| !pc.rels.is_empty());

    // ---- Eligible filter conjuncts join the pool ------------------------
    // For INNER joins a multi-relation WHERE conjunct IS a join condition.
    // Subquery-carrying conjuncts stay at the top (their hidden outer
    // references need the full combined row — pushdown's rule).
    let mut pooled_from_filter = 0usize;
    if let Some(f) = top_filter {
        for c in split_and_chain(f) {
            if crate::executor::expr_has_subquery(&c) {
                top_conjuncts.push(c);
                continue;
            }
            let mut expr = c.clone();
            let mut rels = Vec::new();
            if rewrite_all_refs(&mut expr, 0, n, &atom_cols, &atom_names, &mut rels)
                && rels.len() >= 2
            {
                pool.push(PooledConjunct { expr, rels });
                pooled_from_filter += 1;
            } else {
                top_conjuncts.push(c);
            }
        }
    }

    // Nothing pooled at all (a pure cross-product spine with no WHERE):
    // there is no join order to improve — the greedy would just shuffle
    // cartesian factors and pay a restoration Project for nothing.
    if pool.is_empty() {
        if dbg {
            eprintln!("[reorder] bail: empty pool");
        }
        return None;
    }
    if dbg {
        eprintln!("[reorder] pool: {} conjuncts", pool.len());
        for pc in &pool {
            eprintln!("[reorder]   rels={:?}", pc.rels);
        }
    }

    // ---- Estimates ------------------------------------------------------
    let ests: Vec<i64> = atom_plans
        .iter()
        .map(|a| atom_est_rows(catalog, a))
        .collect();

    // ---- Cost-searched DP rebuild (spines within the cap) ----------------
    // The DP explores EVERY left-deep order (and, within the bushy cap,
    // every partitioned tree) under a selectivity-aware cost model — a
    // strict superset of the greedy's single smallest-atom-first guess
    // below. The syntactic (identity) order is inside the search space,
    // so "no win" means identity-optimal: decline and keep the syntactic
    // tree. The greedy only serves spines beyond the DP cap.
    // RSQL_NO_DP=1 disables the DP (A/B debugging knob — the greedy takes
    // over at every spine size).
    if n <= DP_LEFTDEEP_MAX_ATOMS && std::env::var_os("RSQL_NO_DP").is_none() {
        return try_cost_search_spine(
            catalog,
            &atom_plans,
            &atom_cols,
            pool,
            &ests,
            pooled_from_filter,
            top_conjuncts,
        );
    }

    // ---- Greedy connected left-deep rebuild ------------------------------
    let mut covered = vec![false; n];
    let seed = (0..n).min_by_key(|&i| (ests[i], i))?;
    covered[seed] = true;
    let mut order: Vec<usize> = vec![seed];
    let mut acc = atom_plans[seed].clone();

    while order.len() < n {
        // Prefer the smallest atom CONNECTED to the covered set by a live
        // pool conjunct; fall back to the smallest overall (true cross
        // product region).
        let connected: Vec<usize> = (0..n)
            .filter(|&i| {
                !covered[i]
                    && pool
                        .iter()
                        .any(|pc| pc.rels.contains(&i) && pc.rels.iter().any(|&r| covered[r]))
            })
            .collect();
        let candidates: Vec<usize> = if connected.is_empty() {
            (0..n).filter(|&i| !covered[i]).collect()
        } else {
            connected
        };
        let next = *candidates.iter().min_by_key(|&&i| (ests[i], i))?;

        covered[next] = true;
        order.push(next);

        // Attach every pool conjunct whose relations just became covered —
        // the earliest join node that can evaluate it.
        let mut attach: Vec<Expr> = Vec::new();
        let mut rest: Vec<PooledConjunct> = Vec::with_capacity(pool.len());
        for pc in pool.drain(..) {
            if pc.rels.iter().all(|&r| covered[r]) {
                attach.push(pc.expr);
            } else {
                rest.push(pc);
            }
        }
        pool = rest;

        let condition = if attach.is_empty() {
            None
        } else {
            Some(combine_and(&attach))
        };
        let join_type = if condition.is_none() {
            JoinType::Cross
        } else {
            JoinType::Inner
        };
        let algorithm = if condition.as_ref().is_some_and(condition_has_equi_leaf) {
            JoinAlgorithm::Hash
        } else {
            JoinAlgorithm::NestedLoop
        };
        acc = Plan::Join {
            left: Box::new(acc),
            right: Box::new(atom_plans[next].clone()),
            join_type,
            condition,
            algorithm,
        };
    }

    // ---- No-op detection -------------------------------------------------
    if order == (0..n).collect::<Vec<usize>>() && pooled_from_filter == 0 {
        // Greedy outcome is the syntactic order and the filter contributed
        // nothing new: the rebuilt tree would be an expensive clone of the
        // original — keep the original plan object.
        return None;
    }

    // ---- Column-order restoration + top assembly --------------------------
    // (shared with the cost-searched path)
    let leftover: Vec<Expr> = pool.into_iter().map(|pc| pc.expr).collect();
    Some(finish_reordered_spine(
        acc,
        &order,
        &atom_cols,
        leftover,
        top_conjuncts,
    ))
}

/// Assemble the final rebuilt-spine plan: restore the original FROM-clause
/// column order with a bare-reference projection (names are unique by the
/// spine gate, so exact-match resolution is order-independent), then lay
/// any leftover pool conjuncts (relations outside the spine — unreachable
/// in valid SQL, but kept verbatim rather than dropped) and the top-filter
/// conjuncts on top as one Filter.
fn finish_reordered_spine(
    acc: Plan,
    order: &[usize],
    atom_cols: &[Vec<String>],
    leftover: Vec<Expr>,
    top_conjuncts: Vec<Expr>,
) -> Plan {
    let original_names: Vec<String> = atom_cols.iter().flatten().cloned().collect();
    let new_order_names: Vec<String> = order
        .iter()
        .flat_map(|&i| atom_cols[i].iter().cloned())
        .collect();
    let restored = if original_names == new_order_names {
        acc
    } else {
        let columns: Vec<crate::planner::plan::ProjectExpr> = original_names
            .iter()
            .map(|name| crate::planner::plan::ProjectExpr {
                expr: Expr::Column {
                    table: None,
                    name: name.clone(),
                },
                alias: Some(name.clone()),
            })
            .collect();
        Plan::Project {
            input: Box::new(acc),
            columns,
        }
    };

    let mut tops = top_conjuncts;
    tops.extend(leftover);
    if tops.is_empty() {
        restored
    } else {
        Plan::Filter {
            input: Box::new(restored),
            predicate: combine_and(&tops),
        }
    }
}

// ============================================================================
// Cost-searched join ordering (DP over relation subsets)
// ============================================================================
//
// The greedy rebuild above orders joins by ATOM SIZE — smallest relation
// first. That misses the orders a real cost model sees: an equi conjunct's
// SELECTIVITY (how much the join shrinks its sides), the join STEP cost
// (hash build+probe vs. index-driven probes), and BUSHY trees — pairing
// two selective sub-joins instead of growing one left-deep accumulator.
// SQLite's planner searches orders with a full cost model; this DP is the
// engine's answer, and it goes one step further than SQLite (bushy trees).
//
// The search: classic Selinger-style subset DP. For every subset S of the
// spine's relations, keep the best (cost, rows, shape-flag, split) found;
// transitions join two disjoint covered subsets. n <= DP_BUSHY_MAX_ATOMS
// enumerates ALL splits (bushy included, 3^n); n <= DP_LEFTDEEP_MAX_ATOMS
// restricts transitions to (covered prefix ⋈ single atom) — left-deep only
// (2^n × n); beyond that the greedy takes over.
//
// The cost model (per join step joining side 1 of r1 rows to side 2 of r2
// rows, with the conjuncts whose relations span both sides attaching here):
// - output rows = r1 × r2 × Π conjunct selectivity, floored at 1.
//   An equi conjunct `a.x = b.y` contributes 1/max(D(ax), D(by)) where D is
//   the column's stat1 distinct count (ANALYZE) or the rowid-alias row
//   count; without stats it degrades to SQLite's blind 1/10. Non-equi
//   conjuncts use `conjunct_selectivity` (range 1/3, LIKE 1/4, …).
// - pure cartesian (no attaching conjunct): step cost = r1 × r2 (a nested
//   loop over every pair — punished hard, exactly what makes the DP avoid
//   disconnected picks unless the query really is a cross product).
// - hash join (≥1 attaching equi pair): step cost = r1 + r2 + out (build
//   the smaller side, probe the bigger, emit the output).
// - index-driven nested loop (INLJ): when one side is a single BARE SCAN
//   atom whose join column carries an index and the other side is
//   "selective-shaped" (the post-pass INLJ rewrite's own gate — pushed
//   point/range lookups, filters, and already-converted INLJ chains), the
//   step cost is outer_rows × INLJ_PROBE_ROWS + out and the inner atom's
//   scan base cost is NOT paid (it is probed, never read). This mirrors
//   `optimize_index_nested_loop_join` exactly: the rewrite fires on the
//   same eligibility, so the DP's cost and the executed plan agree.
//
// The output rows and the shape-flag propagate: a step's rows feed the
// next step's cost (intermediate sizes compound), and the flag tells the
// next step whether the outer side would be INLJ-convertible (the chain
// rule of the post-pass).
//
// Semantics are the greedy's: inner joins are commutative/associative, the
// multiset of combined rows is order-independent, every pooled conjunct
// attaches at the LOWEST join node covering its relations, and column
// order is restored by the shared `finish_reordered_spine` projection.
//
// No-op gate: the identity (syntactic FROM order) left-deep chain lives in
// the search space, so the DP's best cost is ≤ the identity's under the
// same model, with equal floats when identity is optimal. Rebuild only on
// a real win (>1%) or when the WHERE fusion contributed conjuncts (the
// rebuild fuses them into join nodes — its own optimization).

/// Spine sizes where the full bushy subset DP runs: 3^n split evaluations
/// (≈59k at the cap — microseconds-to-milliseconds at plan time).
const DP_BUSHY_MAX_ATOMS: usize = 10;

/// Spine sizes where the left-deep-only subset DP runs: 2^n × n
/// transitions (≈1M at the cap). Beyond this the greedy heuristic serves.
const DP_LEFTDEEP_MAX_ATOMS: usize = 16;

/// Per-probe cost of an index nested-loop seek, in row-equivalents: a
/// B-tree seek touches a root, an interior, and a leaf page (vs. one row
/// of a sequential scan). Mirrors the post-pass INLJ rewrite's economics.
const INLJ_PROBE_ROWS: f64 = 3.0;

/// Relative win threshold before the DP rebuilds a spine: below this the
/// restoration projection's own per-row cost eats the difference.
const DP_WIN_FACTOR: f64 = 0.99;

/// One pooled conjunct in the DP's cost-model view: the (already
/// canonicalized) expression, its relation mask, and — when it is a bare
/// column-to-column equality over exactly two relations — the equi pair
/// with each side's owning atom and column.
struct DpConjunct {
    expr: Expr,
    mask: u64,
    eq: Option<((usize, String), (usize, String))>,
}

/// Everything the DP needs about the spine, precomputed once.
struct DpCtx<'a> {
    catalog: &'a Catalog,
    /// Atom plans (single-atom conjuncts folded in as Filter wraps).
    plans: Vec<Plan>,
    conjuncts: Vec<DpConjunct>,
    /// Estimated output rows per atom (folds applied).
    rows: Vec<f64>,
    /// Production cost per atom — rows read to produce its output.
    base: Vec<f64>,
    /// `outer_is_selective` per atom (INLJ chain gate).
    selective: Vec<bool>,
    /// The atom's table when its plan is EXACTLY a bare `Scan` (the INLJ
    /// rewrite requires a bare scan as the inner side — a Filter-wrapped
    /// or lookup-shaped atom is hashed, not probed).
    bare_scan: Vec<Option<Arc<Table>>>,
    /// Whether the atom's output exposes table columns positionally (the
    /// INLJ rewrite resolves the outer key column through it).
    key_resolvable: Vec<bool>,
}

impl<'a> DpCtx<'a> {
    /// The stat1 distinct count of `atom`'s column `col`: the rowid-alias
    /// row count, the first index (leading column = `col`) with ANALYZE
    /// stats, else unknown (caller falls back to the blind factor).
    fn distinct(&self, atom: usize, col: &str) -> Option<i64> {
        let plan = &self.plans[atom];
        let table = atom_table_of(plan)?;
        if let Some(idx) = table.rowid_alias {
            if table.columns[idx].name.eq_ignore_ascii_case(col) {
                return Some(table_rows_hint(self.catalog, table));
            }
        }
        for idx in self.catalog.indexes_on_table(&table.name) {
            if idx
                .columns
                .first()
                .is_some_and(|c| c.name.eq_ignore_ascii_case(col))
            {
                if let Some(s) = self.catalog.index_stats(&idx.name) {
                    if let Some(&d) = s.distinct_prefix.first() {
                        if d > 0 {
                            return Some(d);
                        }
                    }
                }
            }
        }
        None
    }

    /// Evaluate one join step: join side `s1` (cost `c1`, rows `r1`, flag
    /// `fl1`) to disjoint side `s2` (`c2`, `r2`, `fl2`). Returns the new
    /// cell (total cost, output rows, output shape flag). Conjuncts whose
    /// masks span both sides attach HERE (masks fully inside one side
    /// attach deeper, by construction).
    #[allow(clippy::too_many_arguments)]
    fn step(
        &self,
        s1: u64,
        s2: u64,
        c1: f64,
        r1: f64,
        fl1: bool,
        c2: f64,
        r2: f64,
        fl2: bool,
    ) -> (f64, f64, bool) {
        let t = s1 | s2;
        let mut sel_prod = 1.0f64;
        let mut has_eq = false;
        let mut has_attaching = false;
        for c in &self.conjuncts {
            let m = c.mask;
            if m & !t == 0 && m & s1 != 0 && m & s2 != 0 {
                has_attaching = true;
                match &c.eq {
                    Some(((a1, col1), (a2, col2))) => {
                        has_eq = true;
                        let d1 = self.distinct(*a1, col1);
                        let d2 = self.distinct(*a2, col2);
                        let d = match (d1, d2) {
                            (Some(x), Some(y)) => Some(x.max(y)),
                            (Some(x), None) | (None, Some(x)) => Some(x),
                            (None, None) => None,
                        };
                        sel_prod *= match d {
                            Some(d) if d > 0 => 1.0 / d as f64,
                            _ => 0.1, // SQLite's blind equality guess
                        };
                    }
                    None => sel_prod *= conjunct_selectivity(&c.expr),
                }
            }
        }
        let out = (r1 * r2 * sel_prod).max(1.0);

        if has_eq {
            // INLJ: a single bare-scan side probed through an index by a
            // selective-shaped outer — the post-pass rewrite's own rule.
            // The inner atom's scan cost is skipped (probed, not read).
            if s2.count_ones() == 1 {
                let j = s2.trailing_zeros() as usize;
                if let Some(tbl) = &self.bare_scan[j] {
                    if fl1 && self.eq_key_indexed(j, tbl, s1, t) {
                        return (
                            c1 + c2 + r1 * INLJ_PROBE_ROWS + out - self.base[j],
                            out,
                            true,
                        );
                    }
                }
            }
            if s1.count_ones() == 1 {
                let j = s1.trailing_zeros() as usize;
                if let Some(tbl) = &self.bare_scan[j] {
                    if fl2 && self.eq_key_indexed(j, tbl, s2, t) {
                        return (
                            c1 + c2 + r2 * INLJ_PROBE_ROWS + out - self.base[j],
                            out,
                            true,
                        );
                    }
                }
            }
            // Hash: build one side, probe the other, emit the output.
            (c1 + c2 + r1 + r2 + out, out, false)
        } else if has_attaching {
            // Attaching non-equi conjuncts: nested loop over every pair.
            (c1 + c2 + r1 * r2 + out, out, false)
        } else {
            // Pure cross product.
            let out = r1 * r2;
            (c1 + c2 + out, out, false)
        }
    }

    /// Does an attaching equi conjunct bind inner atom `j`'s indexed
    /// column against an outer-side atom that exposes its key? Mirrors the
    /// post-pass's pair scan (first pair whose outer key resolves and whose
    /// inner column has an index).
    fn eq_key_indexed(&self, j: usize, tbl: &Table, outer: u64, t: u64) -> bool {
        self.conjuncts.iter().any(|c| {
            let m = c.mask;
            if m & !t != 0 || m & (1u64 << j) == 0 {
                return false; // not attaching here / doesn't touch j
            }
            if let Some(((a1, col1), (a2, col2))) = &c.eq {
                let (other, inner_col) = if *a1 == j {
                    (*a2, col1)
                } else if *a2 == j {
                    (*a1, col2)
                } else {
                    return false;
                };
                (1u64 << other) & outer != 0
                    && self.key_resolvable[other]
                    && find_index_for_column(self.catalog, tbl, inner_col).is_some()
            } else {
                false
            }
        })
    }
}

/// The table an atom's plan reads, when it is table-shaped (Scan family or
/// a Filter over one). CTE rows, table functions and VALUES report None.
fn atom_table_of(plan: &Plan) -> Option<&Table> {
    match plan {
        Plan::Scan { table, .. }
        | Plan::RowidLookup { table, .. }
        | Plan::RowidIn { table, .. }
        | Plan::IndexIn { table, .. }
        | Plan::IndexLookup { table, .. }
        | Plan::IndexRange { table, .. }
        | Plan::RowidRange { table, .. } => Some(table),
        Plan::Filter { input, .. } => atom_table_of(input),
        _ => None,
    }
}

/// Whether an atom's output resolves table columns positionally — the
/// coverage of `resolve_outer_col_index` for single plans.
fn atom_exposes_table_cols(plan: &Plan) -> bool {
    match plan {
        Plan::Scan { .. }
        | Plan::RowidLookup { .. }
        | Plan::RowidIn { .. }
        | Plan::IndexIn { .. }
        | Plan::IndexLookup { .. }
        | Plan::IndexRange { .. }
        | Plan::RowidRange { .. } => true,
        Plan::Filter { input, .. } => atom_exposes_table_cols(input),
        _ => false,
    }
}

/// Production cost of an atom's access path — the rows READ to produce
/// its output (`atom_est_rows` is the rows EMITTED). Filter pays the full
/// inner read plus emits the filtered rows; lookups pay their seek fan-in.
fn atom_base_cost(catalog: &Catalog, plan: &Plan) -> f64 {
    match plan {
        Plan::RowidLookup { .. } => 1.0,
        Plan::RowidIn { values, .. } => (values.len() as f64).max(1.0),
        Plan::RowidRange {
            table, start, end, ..
        } => {
            let lit = |e: &Option<Expr>| match e.as_ref() {
                Some(Expr::Literal(Value::Integer(i))) => Some(*i),
                _ => None,
            };
            match (lit(start), lit(end)) {
                (Some(s), Some(e)) if e >= s => {
                    ((e - s + 1).min(table_rows_hint(catalog, table)) as f64).max(1.0)
                }
                _ => (table_rows_hint(catalog, table) as f64 / 4.0).max(1.0),
            }
        }
        Plan::IndexLookup {
            index, key_exprs, ..
        } => {
            let k = key_exprs.len();
            catalog
                .index_stats(&index.name)
                .map(|s| s.estimate_eq(k))
                .filter(|&e| e > 0)
                .unwrap_or(10) as f64
        }
        Plan::IndexIn {
            index, key_exprs, ..
        } => {
            let members = key_exprs.len() as f64;
            let base = catalog
                .index_stats(&index.name)
                .map(|s| s.estimate_eq(1))
                .filter(|&e| e > 0)
                .unwrap_or(10) as f64;
            (base * members).max(1.0)
        }
        Plan::IndexRange { index, .. } => catalog
            .index_stats(&index.name)
            .map(|s| s.rows / 4)
            .filter(|&e| e > 0)
            .unwrap_or(UNANALYZED_ROWS / 4) as f64,
        Plan::Filter { input, .. } => {
            atom_base_cost(catalog, input) + atom_est_rows(catalog, input) as f64
        }
        Plan::Scan { table, .. } => table_rows_hint(catalog, table) as f64,
        Plan::CteRows { rows, .. } => (rows.len() as f64).max(1.0),
        _ => UNANALYZED_ROWS as f64,
    }
}

/// Wrap an atom plan in a Filter (merging when it already is one) — the
/// fold applied to single-atom pooled conjuncts, the same shape pushdown
/// would have produced.
fn wrap_atom_filter(plan: Plan, conj: Expr) -> Plan {
    if let Plan::Filter { input, predicate } = plan {
        let mut conjuncts = split_and_chain(&predicate);
        conjuncts.push(conj);
        Plan::Filter {
            input,
            predicate: combine_and(&conjuncts),
        }
    } else {
        Plan::Filter {
            input: Box::new(plan),
            predicate: conj,
        }
    }
}

/// The (table, name) of a column operand, seeing through COLLATE.
fn operand_col(e: &Expr) -> Option<(&Option<String>, &str)> {
    match e {
        Expr::Column { table, name } => Some((table, name)),
        Expr::Collate { expr, .. } => operand_col(expr),
        _ => None,
    }
}

/// The equi-pair shape of a pooled conjunct: `col = col` with both
/// operands resolving to DISTINCT atoms (a join key pair).
fn eq_pair_of(
    expr: &Expr,
    n: usize,
    atom_cols: &[Vec<String>],
) -> Option<((usize, String), (usize, String))> {
    let Expr::Binary {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    else {
        return None;
    };
    let (lq, ln) = operand_col(left)?;
    let (rq, rn) = operand_col(right)?;
    let (la, _) = resolve_ref_window(lq, ln, 0, n, atom_cols)?;
    let (ra, _) = resolve_ref_window(rq, rn, 0, n, atom_cols)?;
    if la == ra {
        return None; // same-atom equality — not a join key
    }
    let plain = |s: &str| s.rsplit('.').next().unwrap_or(s).to_string();
    Some(((la, plain(ln)), (ra, plain(rn))))
}

/// Cost-searched rebuild of one flattened inner-join spine. Returns None
/// (caller keeps the syntactic tree) when the DP finds no meaningful win
/// and the filter fusion contributed nothing.
#[allow(clippy::too_many_arguments)]
fn try_cost_search_spine(
    catalog: &Catalog,
    atom_plans: &[Plan],
    atom_cols: &[Vec<String>],
    pool: Vec<PooledConjunct>,
    ests: &[i64],
    pooled_from_filter: usize,
    mut top_conjuncts: Vec<Expr>,
) -> Option<Plan> {
    let n = atom_plans.len();
    let dbg = std::env::var_os("RSQL_DBG_REORDER").is_some();

    // ---- Fold single-atom conjuncts into their atoms ---------------------
    // A conjunct referencing one relation evaluates identically as a Filter
    // on the atom (pushdown's own shape) — cheaper than any join-node
    // placement, and it keeps the DP's masks strictly 2+ relation.
    let mut plans: Vec<Plan> = atom_plans.to_vec();
    let mut rows: Vec<f64> = ests.iter().map(|&e| e as f64).collect();
    let mut multi: Vec<PooledConjunct> = Vec::new();
    for pc in pool {
        if pc.rels.len() == 1 {
            let j = pc.rels[0];
            rows[j] = (rows[j] * conjunct_selectivity(&pc.expr)).max(1.0);
            let plan = std::mem::replace(&mut plans[j], Plan::Values { rows: vec![] });
            plans[j] = wrap_atom_filter(plan, pc.expr);
        } else {
            multi.push(pc);
        }
    }

    // ---- Conjunct masks + equi-pair shapes --------------------------------
    let mut conjuncts: Vec<DpConjunct> = Vec::with_capacity(multi.len());
    for pc in multi {
        let mut mask = 0u64;
        for &r in &pc.rels {
            mask |= 1u64 << r;
        }
        let eq = eq_pair_of(&pc.expr, n, atom_cols);
        conjuncts.push(DpConjunct {
            expr: pc.expr,
            mask,
            eq,
        });
    }

    let base: Vec<f64> = plans.iter().map(|p| atom_base_cost(catalog, p)).collect();
    let selective: Vec<bool> = plans.iter().map(outer_is_selective).collect();
    let bare_scan: Vec<Option<Arc<Table>>> = plans
        .iter()
        .map(|p| match p {
            Plan::Scan { table, .. } => Some(table.clone()),
            _ => None,
        })
        .collect();
    let key_resolvable: Vec<bool> = plans.iter().map(atom_exposes_table_cols).collect();

    let ctx = DpCtx {
        catalog,
        plans,
        conjuncts,
        rows,
        base,
        selective,
        bare_scan,
        key_resolvable,
    };

    // ---- Subset DP --------------------------------------------------------
    let full: u64 = (1u64 << n) - 1;
    let sz = 1usize << n;
    let mut cost = vec![f64::INFINITY; sz];
    let mut rows_d = vec![0.0f64; sz];
    let mut flag = vec![false; sz];
    let mut split: Vec<(u64, u64)> = vec![(0, 0); sz];
    for i in 0..n {
        let m = 1u64 << i;
        cost[m as usize] = ctx.base[i];
        rows_d[m as usize] = ctx.rows[i];
        flag[m as usize] = ctx.selective[i];
    }

    // Popcount buckets: every split's sources have smaller popcount, so
    // processing masks in popcount order leaves every source final.
    let mut buckets: Vec<Vec<u64>> = vec![Vec::new(); n + 1];
    for m in 1u64..=full {
        buckets[m.count_ones() as usize].push(m);
    }

    let bushy = n <= DP_BUSHY_MAX_ATOMS;
    // Popcount ≥ 2 buckets, in popcount order (skip the singleton buckets).
    for bucket in buckets.iter().skip(2) {
        for &t in bucket {
            let ti = t as usize;
            if bushy {
                // Every unordered proper split {s1, s2} (bushy included).
                let mut s1 = (t - 1) & t;
                while s1 != 0 {
                    let s2 = t ^ s1;
                    if s1 < s2 {
                        let a = s1 as usize;
                        let b = s2 as usize;
                        let (nc, nr, nf) = ctx.step(
                            s1, s2, cost[a], rows_d[a], flag[a], cost[b], rows_d[b], flag[b],
                        );
                        if nc < cost[ti] {
                            cost[ti] = nc;
                            rows_d[ti] = nr;
                            flag[ti] = nf;
                            split[ti] = (s1, s2);
                        }
                    }
                    s1 = (s1 - 1) & t;
                }
            } else {
                // Left-deep only: covered prefix ⋈ one atom.
                for j in 0..n {
                    let bit = 1u64 << j;
                    if t & bit != 0 {
                        let s1 = t ^ bit;
                        let a = s1 as usize;
                        let (nc, nr, nf) = ctx.step(
                            s1,
                            bit,
                            cost[a],
                            rows_d[a],
                            flag[a],
                            ctx.base[j],
                            ctx.rows[j],
                            ctx.selective[j],
                        );
                        if nc < cost[ti] {
                            cost[ti] = nc;
                            rows_d[ti] = nr;
                            flag[ti] = nf;
                            split[ti] = (s1, bit);
                        }
                    }
                }
            }
        }
    }

    // ---- Identity (syntactic order) chain under the same model ----------
    let mut acc_mask = 1u64;
    let mut c = ctx.base[0];
    let mut r = ctx.rows[0];
    let mut fl = ctx.selective[0];
    for j in 1..n {
        let bit = 1u64 << j;
        let (nc, nr, nfl) = ctx.step(
            acc_mask,
            bit,
            c,
            r,
            fl,
            ctx.base[j],
            ctx.rows[j],
            ctx.selective[j],
        );
        c = nc;
        r = nr;
        fl = nfl;
        acc_mask |= bit;
    }
    let identity_cost = c;

    let best = cost[full as usize];
    let win = best < identity_cost * DP_WIN_FACTOR;
    if dbg {
        let order = dp_order(full, &split);
        eprintln!(
            "[dp] n={n} identity_cost={identity_cost:.0} best={best:.0} win={win} pooled={pooled_from_filter} order={order:?}"
        );
    }
    if !win && pooled_from_filter == 0 {
        return None; // identity-optimal (or within noise): keep the tree
    }

    // ---- Reconstruct the winning tree -------------------------------------
    let mut used = vec![false; ctx.conjuncts.len()];
    let tree = dp_build(full, &ctx, &mut used, &split);
    let order = dp_order(full, &split);
    let leftover: Vec<Expr> = ctx
        .conjuncts
        .iter()
        .enumerate()
        .filter(|(i, _)| !used[*i])
        .map(|(_, c)| c.expr.clone())
        .collect();
    top_conjuncts.extend(leftover);
    Some(finish_reordered_spine(
        tree,
        &order,
        atom_cols,
        Vec::new(),
        top_conjuncts,
    ))
}

/// Build the winning plan for `mask` bottom-up from the DP's split table,
/// attaching each conjunct at the lowest join node covering its relations
/// (exactly the nodes the cost model charged).
fn dp_build(mask: u64, ctx: &DpCtx, used: &mut [bool], split: &[(u64, u64)]) -> Plan {
    if mask.count_ones() == 1 {
        let j = mask.trailing_zeros() as usize;
        return ctx.plans[j].clone();
    }
    let (s1, s2) = split[mask as usize];
    let left = dp_build(s1, ctx, used, split);
    let right = dp_build(s2, ctx, used, split);
    let mut attach: Vec<Expr> = Vec::new();
    for (i, c) in ctx.conjuncts.iter().enumerate() {
        if used[i] {
            continue;
        }
        let m = c.mask;
        if m & !mask == 0 && m & s1 != 0 && m & s2 != 0 {
            attach.push(c.expr.clone());
            used[i] = true;
        }
    }
    let condition = if attach.is_empty() {
        None
    } else {
        Some(combine_and(&attach))
    };
    let join_type = if condition.is_none() {
        JoinType::Cross
    } else {
        JoinType::Inner
    };
    let algorithm = if condition.as_ref().is_some_and(condition_has_equi_leaf) {
        JoinAlgorithm::Hash
    } else {
        JoinAlgorithm::NestedLoop
    };
    Plan::Join {
        left: Box::new(left),
        right: Box::new(right),
        join_type,
        condition,
        algorithm,
    }
}

/// The atom order of the winning tree (left-to-right leaf order) — the
/// restoration projection's input order.
fn dp_order(mask: u64, split: &[(u64, u64)]) -> Vec<usize> {
    if mask.count_ones() == 1 {
        return vec![mask.trailing_zeros() as usize];
    }
    let (s1, s2) = split[mask as usize];
    let mut o = dp_order(s1, split);
    o.extend(dp_order(s2, split));
    o
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
        // Table-valued functions have a fixed output schema per family
        // (json_each/json_tree: 8 columns; pragma_*: 3-8). A stable upper
        // bound keeps Join slot-offset math conservative without binding
        // the executor to this estimate.
        Plan::TableFunction { .. } => 8,
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

// ---------------------------------------------------------------------------
// ROWID-ORDER Sort elision
// ---------------------------------------------------------------------------

/// Drop `Sort` nodes whose ordering is ALREADY the input's row order: a bare
/// table b-tree scan (or a Project/Filter/rowid-range wrapper over one)
/// emits rows in ascending rowid order, so `ORDER BY <rowid-alias>` ASC
/// (including the `rowid` / `_rowid_` / `oid` spellings) is a no-op. Only
/// the two shapes `insert_sort_below_top` produces for scan inputs are
/// rewritten (`Sort(X)` and `Project(Sort(X))`); every other plan passes
/// through untouched. DESC never elides (no reverse scan), index scans
/// never elide (index order, not rowid order).
fn try_elide_rowid_order_sort(plan: Plan) -> Plan {
    match plan {
        Plan::Sort { input, terms } => {
            if sort_terms_are_rowid_order(&terms, &input) {
                // The scan already emits exactly this order: drop the node.
                *input
            } else {
                Plan::Sort { input, terms }
            }
        }
        Plan::Project { input, columns } => Plan::Project {
            input: Box::new(try_elide_rowid_order_sort(*input)),
            columns,
        },
        other => other,
    }
}

// ============================================================================
// rowid-in-expression rewrite (see plan_select's hook for the rationale)
// ============================================================================

/// Hidden rowid slot name for tables WITHOUT a rowid-alias column: the
/// row-producing drivers/executors append the rowid value as a trailing
/// row slot registered under this name, so expression evaluation
/// resolves it by exact match. The NUL prefix makes it unparseable as an
/// SQL identifier (no user column can collide) and lets star expansions
/// filter it out (`SELECT *` must not see it).
pub(crate) const HIDDEN_ROWID: &str = "\u{0}rowid";

/// True for names that are internal hidden slots (star expansion filter).
/// Accepts BOTH spellings: the legacy bare `"\0rowid"` and the
/// side-qualified `"a.\0rowid"` (see `hidden_rowid_slot` — joins need
/// each side's slot distinguishable by name, or `a.rowid` and `b.rowid`
/// both bind to the FIRST hidden slot in the combined list).
pub(crate) fn is_hidden_rowid(name: &str) -> bool {
    name.starts_with('\u{0}') || name.contains(".\u{0}rowid")
}

/// The hidden rowid slot's name for a scan of `table` under the
/// effective prefix (alias or table name): `"prefix.\0rowid"`. The
/// qualifier makes the slot resolvable per-SIDE in a join's combined
/// column list — `a.rowid` binds to `a.\0rowid`, `b.rowid` to
/// `b.\0rowid` — while the NUL marker keeps it unparseable as a user
/// identifier and filtered out of star expansions.
pub(crate) fn hidden_rowid_slot(prefix: &str) -> String {
    format!("{}.{}", prefix, HIDDEN_ROWID)
}

/// Is `name` one of the rowid pseudo-column spellings (`rowid`,
/// `_rowid_`, `oid`, or the hidden slot's own name)?
pub(crate) fn is_rowid_spelling(name: &str) -> bool {
    name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid")
        || name == HIDDEN_ROWID
}

/// Do rows from this table's scans carry the hidden rowid slot? Only
/// no-alias tables need it (alias tables expose the rowid through the
/// alias column; WITHOUT ROWID tables have no rowid; a real "rowid"
/// column shadows the pseudo-column).
pub(crate) fn wants_rowid_slot(table: &crate::schema::Table) -> bool {
    table.rowid_alias.is_none() && !table.without_rowid && table.find_column("rowid").is_none()
}

/// Rewrite `rowid`/`_rowid_`/`oid` column references inside expressions to
/// the table's rowid-alias column, for single-table queries over a table
/// WITH an INTEGER PRIMARY KEY. Scope rules:
/// - A real column named `rowid`/`_rowid_`/`oid` shadows the pseudo-column
///   (SQLite rule): no rewrite.
/// - Qualified references must name the table or its alias.
/// - Subquery / EXISTS bodies are their own scope (their own plan_select
///   pass fires): not descended into.
/// - No rowid-alias column: no in-row value exists (documented gap) —
///   leave the reference (it evaluates NULL, as before).
fn rewrite_rowid_refs_in_plan(plan: &mut Plan, table: &Arc<Table>, alias: Option<&str>) {
    let exprs: Vec<&mut Expr> = match plan {
        Plan::Scan { predicate, .. } => predicate.iter_mut().collect(),
        Plan::RowidLookup { rowid, .. } => vec![rowid],
        Plan::RowidIn {
            values, residual, ..
        } => std::sync::Arc::make_mut(values)
            .iter_mut()
            .chain(residual.iter_mut())
            .collect(),
        Plan::RowidRange {
            start,
            end,
            residual,
            ..
        } => start
            .iter_mut()
            .chain(end.iter_mut())
            .chain(residual.iter_mut())
            .collect(),
        Plan::IndexIn {
            key_exprs,
            residual,
            ..
        } => std::sync::Arc::make_mut(key_exprs)
            .iter_mut()
            .chain(residual.iter_mut())
            .collect(),
        Plan::IndexLookup { key_exprs, .. } => key_exprs.iter_mut().collect(),
        Plan::IndexRange {
            start,
            end,
            residual,
            ..
        } => start
            .iter_mut()
            .map(|(e, _)| e)
            .chain(end.iter_mut().map(|(e, _)| e))
            .chain(residual.iter_mut())
            .collect(),
        Plan::Values { rows } => rows.iter_mut().flatten().collect(),
        Plan::Filter { input, predicate } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            vec![predicate]
        }
        Plan::Project { input, columns } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            columns.iter_mut().map(|c| &mut c.expr).collect()
        }
        Plan::Sort { input, terms } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            terms.iter_mut().map(|t| &mut t.expr).collect()
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            group_by
                .iter_mut()
                .chain(aggregates.iter_mut().filter_map(|a| a.arg.as_mut()))
                .collect()
        }
        Plan::Window { input, windows } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            let mut out: Vec<&mut Expr> = Vec::new();
            for w in windows.iter_mut() {
                if let Some(a) = w.arg.as_mut() {
                    out.push(a);
                }
                out.extend(w.extra_args.iter_mut());
                out.extend(w.partition_by.iter_mut());
                out.extend(w.order_by.iter_mut().map(|t| &mut t.expr));
            }
            out
        }
        Plan::TableFunction { args, .. } => args.iter_mut().collect(),
        Plan::Distinct { input } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            Vec::new()
        }
        // Subquery plans are NOT rewritten here: their expressions belong
        // to their own FROM scope (their own plan_select pass fired with
        // their own table).
        Plan::Subquery { .. } => Vec::new(),
        Plan::Limit { input, .. } => {
            rewrite_rowid_refs_in_plan(input, table, alias);
            Vec::new()
        }
        // Join conditions: bare `rowid` is ambiguous across tables in
        // general — leave them (rewrite fires only for single-table FROM
        // anyway, so this is defense-in-depth).
        Plan::Join { left, right, .. } => {
            rewrite_rowid_refs_in_plan(left, table, alias);
            rewrite_rowid_refs_in_plan(right, table, alias);
            Vec::new()
        }
        Plan::IndexNestedLoopJoin { outer, .. } => {
            rewrite_rowid_refs_in_plan(outer, table, alias);
            Vec::new()
        }
        // DML / CTE rows / set operations: not reachable from a plain
        // SELECT's plan walk (DML plans are built elsewhere; compound
        // selects skip the rewrite at the plan_select hook).
        _ => Vec::new(),
    };
    for e in exprs {
        rewrite_rowid_in_expr(e, table, alias);
    }
}

/// MULTI-SIDE rowid-in-expression rewrite: a join's FROM clause
/// contributes several (table, alias) sides, and a QUALIFIED rowid ref
/// (`u.rowid`, `o.oid`) names exactly one of them. Each expression site
/// in the plan rewrites through every side; the qualifier check inside
/// makes non-matching sides no-ops. Bare spellings are left alone (bare
/// `rowid` across join sides is ambiguous — the evaluator's first-slot
/// rule and SQLite's ambiguity latitude apply).
///
/// This is what makes `ON u.rowid = o.u_id` (an INTEGER-PK alias table
/// on one side) resolve inside joins: the single-table hook never fires
/// for a multi-source FROM, so the ref previously evaluated NULL.
/// Scope boundaries (subquery plans) are not descended into — their own
/// plan_select pass fires with their own FROM sides.
fn rewrite_rowid_refs_in_plan_multi(
    plan: &mut Plan,
    sides: &[(std::sync::Arc<Table>, Option<String>)],
) {
    let exprs: Vec<&mut Expr> = match plan {
        Plan::Scan { predicate, .. } => predicate.iter_mut().collect(),
        Plan::RowidLookup { rowid, .. } => vec![rowid],
        Plan::RowidIn {
            values, residual, ..
        } => std::sync::Arc::make_mut(values)
            .iter_mut()
            .chain(residual.iter_mut())
            .collect(),
        Plan::RowidRange {
            start,
            end,
            residual,
            ..
        } => start
            .iter_mut()
            .chain(end.iter_mut())
            .chain(residual.iter_mut())
            .collect(),
        Plan::IndexIn {
            key_exprs,
            residual,
            ..
        } => std::sync::Arc::make_mut(key_exprs)
            .iter_mut()
            .chain(residual.iter_mut())
            .collect(),
        Plan::IndexLookup { key_exprs, .. } => key_exprs.iter_mut().collect(),
        Plan::IndexRange {
            start,
            end,
            residual,
            ..
        } => start
            .iter_mut()
            .map(|(e, _)| e)
            .chain(end.iter_mut().map(|(e, _)| e))
            .chain(residual.iter_mut())
            .collect(),
        Plan::Values { rows } => rows.iter_mut().flatten().collect(),
        Plan::Filter { input, predicate } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            vec![predicate]
        }
        Plan::Project { input, columns } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            columns.iter_mut().map(|c| &mut c.expr).collect()
        }
        Plan::Sort { input, terms } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            terms.iter_mut().map(|t| &mut t.expr).collect()
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            group_by
                .iter_mut()
                .chain(aggregates.iter_mut().filter_map(|a| a.arg.as_mut()))
                .collect()
        }
        Plan::Window { input, windows } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            let mut out: Vec<&mut Expr> = Vec::new();
            for w in windows.iter_mut() {
                if let Some(a) = w.arg.as_mut() {
                    out.push(a);
                }
                out.extend(w.extra_args.iter_mut());
                out.extend(w.partition_by.iter_mut());
                out.extend(w.order_by.iter_mut().map(|t| &mut t.expr));
            }
            out
        }
        Plan::TableFunction { args, .. } => args.iter_mut().collect(),
        Plan::Distinct { input } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            Vec::new()
        }
        // Subquery plans are their own FROM scope.
        Plan::Subquery { .. } => Vec::new(),
        Plan::Limit { input, .. } => {
            rewrite_rowid_refs_in_plan_multi(input, sides);
            Vec::new()
        }
        Plan::Join {
            left,
            right,
            condition,
            ..
        } => {
            rewrite_rowid_refs_in_plan_multi(left, sides);
            rewrite_rowid_refs_in_plan_multi(right, sides);
            condition.iter_mut().collect()
        }
        Plan::IndexNestedLoopJoin { outer, .. } => {
            rewrite_rowid_refs_in_plan_multi(outer, sides);
            Vec::new()
        }
        // DML / CTE rows / set operations: not reachable from a plain
        // SELECT plan walk.
        _ => Vec::new(),
    };
    for e in exprs {
        for (t, a) in sides {
            rewrite_rowid_in_expr_qual(e, t, a.as_deref());
        }
    }
}

/// Collect every base-table (name, alias) side of a FROM expression
/// tree — walking Join nodes, skipping subqueries and table functions
/// (their output columns carry no rowid slots; a same-named qualifier
/// simply matches nothing in the rewrite).
fn collect_from_sides(from: &TableExpression, out: &mut Vec<(String, Option<String>)>) {
    match from {
        TableExpression::Table { name, alias, .. } => {
            out.push((name.clone(), alias.clone()));
        }
        TableExpression::Join { left, right, .. } => {
            collect_from_sides(left, out);
            collect_from_sides(right, out);
        }
        TableExpression::Subquery { .. } | TableExpression::Function { .. } => {}
    }
}

/// One expression tree: rewrite rowid spellings everywhere EXCEPT inside
/// subquery bodies (scope boundary — see the plan-level doc above).
fn rewrite_rowid_in_expr(e: &mut Expr, table: &Arc<Table>, alias: Option<&str>) {
    rewrite_rowid_in_expr_inner(e, table, alias, false)
}

/// Qualified-only variant for MULTI-SIDE (join) scopes: bare `rowid` is
/// ambiguous across join sides, so only refs that name a side are
/// rewritten — bare refs stay for the evaluator's first-slot rule.
fn rewrite_rowid_in_expr_qual(e: &mut Expr, table: &Arc<Table>, alias: Option<&str>) {
    rewrite_rowid_in_expr_inner(e, table, alias, true)
}

fn rewrite_rowid_in_expr_inner(
    e: &mut Expr,
    table: &Arc<Table>,
    alias: Option<&str>,
    qual_only: bool,
) {
    match e {
        Expr::Column { table: qual, name } => {
            match qual {
                Some(q) => {
                    let effective = alias.unwrap_or(&table.name);
                    if !q.eq_ignore_ascii_case(&table.name) && !q.eq_ignore_ascii_case(effective) {
                        return;
                    }
                }
                // Multi-side scope: bare spellings are ambiguous — skip.
                None if qual_only => return,
                None => {}
            }
            let is_spelling = name.eq_ignore_ascii_case("rowid")
                || name.eq_ignore_ascii_case("_rowid_")
                || name.eq_ignore_ascii_case("oid");
            if !is_spelling {
                return;
            }
            // A real column with the spelling's name shadows the
            // pseudo-column (SQLite resolution rule).
            if table.find_column(name).is_some() {
                return;
            }
            if let Some(idx) = table.rowid_alias {
                // Multi-side scope: keep the side's EFFECTIVE prefix as
                // the qualifier — a bare alias-column name would be
                // ambiguous against the other side's same-named column
                // (`u.rowid`/`o.rowid` over two INTEGER-PK tables both
                // rewriting to bare "id").
                *e = if qual_only {
                    Expr::Column {
                        table: Some(alias.unwrap_or(&table.name).to_string()),
                        name: table.columns[idx].name.clone(),
                    }
                } else {
                    Expr::Column {
                        table: None,
                        name: table.columns[idx].name.clone(),
                    }
                };
            } else if !table.without_rowid {
                // No alias column: the scan drivers append the rowid as a
                // trailing slot named "<prefix>.\0rowid" (see
                // plan_select's hook and `hidden_rowid_slot`) — the
                // QUALIFIED name resolves both in single-table scopes
                // (suffix pass) and inside JOINs, where the combined
                // column list carries BOTH sides' slots and a bare
                // "\0rowid" ref would always bind the first one.
                *e = Expr::Column {
                    table: None,
                    name: hidden_rowid_slot(alias.unwrap_or(&table.name)),
                };
            }
        }
        Expr::Binary { left, right, .. } => {
            rewrite_rowid_in_expr_inner(left, table, alias, qual_only);
            rewrite_rowid_in_expr_inner(right, table, alias, qual_only);
        }
        Expr::Unary { expr, .. } => rewrite_rowid_in_expr_inner(expr, table, alias, qual_only),
        Expr::Between {
            expr, low, high, ..
        } => {
            rewrite_rowid_in_expr_inner(expr, table, alias, qual_only);
            rewrite_rowid_in_expr_inner(low, table, alias, qual_only);
            rewrite_rowid_in_expr_inner(high, table, alias, qual_only);
        }
        Expr::In {
            expr,
            source: InSource::List(values),
            ..
        } => {
            rewrite_rowid_in_expr_inner(expr, table, alias, qual_only);
            for v in std::sync::Arc::make_mut(values).iter_mut() {
                rewrite_rowid_in_expr_inner(v, table, alias, qual_only);
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            rewrite_rowid_in_expr_inner(expr, table, alias, qual_only);
            rewrite_rowid_in_expr_inner(pattern, table, alias, qual_only);
            if let Some(es) = escape.as_mut() {
                rewrite_rowid_in_expr_inner(es, table, alias, qual_only);
            }
        }
        Expr::IsNull { expr, .. } => rewrite_rowid_in_expr_inner(expr, table, alias, qual_only),
        Expr::Is { left, right, .. } => {
            rewrite_rowid_in_expr_inner(left, table, alias, qual_only);
            rewrite_rowid_in_expr_inner(right, table, alias, qual_only);
        }
        Expr::Function { args, filter, .. } => {
            for a in args.iter_mut() {
                rewrite_rowid_in_expr_inner(a, table, alias, qual_only);
            }
            if let Some(f) = filter.as_mut() {
                rewrite_rowid_in_expr_inner(f, table, alias, qual_only);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand.as_mut() {
                rewrite_rowid_in_expr_inner(o, table, alias, qual_only);
            }
            for (w, t) in whens.iter_mut() {
                rewrite_rowid_in_expr_inner(w, table, alias, qual_only);
                rewrite_rowid_in_expr_inner(t, table, alias, qual_only);
            }
            if let Some(el) = else_.as_mut() {
                rewrite_rowid_in_expr_inner(el, table, alias, qual_only);
            }
        }
        Expr::Row(exprs) => {
            for x in exprs.iter_mut() {
                rewrite_rowid_in_expr_inner(x, table, alias, qual_only);
            }
        }
        Expr::Cast { expr, .. } => rewrite_rowid_in_expr_inner(expr, table, alias, qual_only),
        Expr::Collate { expr, .. } => rewrite_rowid_in_expr_inner(expr, table, alias, qual_only),
        // Scope boundaries: subqueries plan their own rowid scope.
        Expr::Subquery(_) | Expr::Exists(_) => {}
        Expr::In {
            source: InSource::Subquery(_),
            ..
        } => {}
        // Literals / parameters / RAISE messages carry no column refs.
        Expr::Literal(_)
        | Expr::Parameter(_)
        | Expr::In {
            source: InSource::Table(_),
            ..
        }
        | Expr::Raise { .. } => {}
    }
}

/// True when a single ASC term names the rowid-alias key of the
/// (order-preserving) plan `input`. `input` must itself emit rowid order
/// (bare table scan / rowid range / Project or Filter over one of those).
fn sort_terms_are_rowid_order(terms: &[OrderTerm], input: &Plan) -> bool {
    if terms.len() != 1 {
        return false;
    }
    let term = &terms[0];
    if term.order != Order::Asc {
        return false;
    }
    // The term must name the scan's rowid-alias column, qualified or not.
    let Expr::Column { table: qual, name } = &term.expr else {
        return false;
    };
    let (table, alias) = match scan_table_of(input) {
        Some(ta) => ta,
        None => return false,
    };
    if let Some(q) = qual {
        let effective = alias.as_deref().unwrap_or(&table.name);
        if !q.eq_ignore_ascii_case(effective) {
            return false;
        }
    }
    // `rowid` / `_rowid_` / `oid` synonyms and the alias column itself.
    let is_rowid_name = name.eq_ignore_ascii_case("rowid")
        || name.eq_ignore_ascii_case("_rowid_")
        || name.eq_ignore_ascii_case("oid");
    match table.rowid_alias {
        Some(idx) => {
            let alias_matches = table.columns[idx].name.eq_ignore_ascii_case(name);
            // A rowid SPELLING is safe only when no other real column
            // carries that name (`CREATE TABLE t (id INTEGER PRIMARY KEY,
            // _rowid_ TEXT)` — `ORDER BY _rowid_` means the TEXT column).
            let spelling_ok = is_rowid_name
                && !table
                    .columns
                    .iter()
                    .enumerate()
                    .any(|(i, c)| i != idx && c.name.eq_ignore_ascii_case(name));
            alias_matches || spelling_ok
        }
        None => {
            // No alias column: only a rowid spelling can denote the rowid
            // itself — and again only when no real column owns the name.
            is_rowid_name
                && !table
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(name))
        }
    }
}

/// Find the (table, alias) of the single bare table scan feeding `input`,
/// through order-preserving wrappers. `None` for anything else (index
/// scans, joins, aggregates, subqueries, vtabs).
fn scan_table_of(input: &Plan) -> Option<(std::sync::Arc<Table>, Option<String>)> {
    match input {
        Plan::Scan {
            table,
            alias,
            index: None,
            predicate: _,
        } if table.vtab.is_none() => Some((table.clone(), alias.clone())),
        Plan::RowidRange { table, alias, .. } => Some((table.clone(), alias.clone())),
        Plan::Project { input, .. } | Plan::Filter { input, .. } => scan_table_of(input),
        _ => None,
    }
}

/// Insert a Sort node below the topmost Project / Distinct node in the plan,
/// so the Sort can see all input columns (not just the projected ones).
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
        TableExpression::Function { .. } => {}
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

// ---------------------------------------------------------------------------
// Index-selection collation gating
// ---------------------------------------------------------------------------

/// The effective collation a comparison conjunct compares under, or `None`
/// for BINARY. WHERE / HAVING / join predicates pass through
/// `rewrite_column_collations` BEFORE index selection, so a comparison
/// that involves a column with a DECLARED collation (or an explicit
/// `COLLATE`) carries an `Expr::Collate` on an operand here — recover it.
/// SQLite's rule: explicit COLLATE on either operand wins, else the
/// column's declared collation, else BINARY.
pub(crate) fn conjunct_collation(conjunct: &Expr) -> Option<String> {
    match conjunct {
        Expr::Binary { left, right, .. } => {
            operand_collation(left.as_ref()).or_else(|| operand_collation(right.as_ref()))
        }
        Expr::Between {
            expr, low, high, ..
        } => operand_collation(expr.as_ref())
            .or_else(|| operand_collation(low.as_ref()))
            .or_else(|| operand_collation(high.as_ref())),
        Expr::In { expr, .. } => operand_collation(expr.as_ref()),
        _ => None,
    }
}

fn operand_collation(e: &Expr) -> Option<String> {
    if let Expr::Collate { collation, .. } = e {
        Some(collation.clone())
    } else {
        None
    }
}

/// Can an index column serve a comparison conjunct? SQLite uses an index
/// to satisfy a WHERE term only when the index column's collation MATCHES
/// the comparison's collation: a NOCASE index cannot seek a BINARY
/// equality (its key order folds case — `v = 'apple'` would wrongly match
/// 'APPLE'), and a BINARY index cannot seek a NOCASE comparison (the
/// probe misses case variants). Mismatched pairs fall back to the scan +
/// Filter paths, which evaluate the comparison under its own collation.
pub(crate) fn index_column_serves_conjunct(
    conjunct: &Expr,
    index_col: &crate::schema::IndexColumn,
) -> bool {
    match conjunct_collation(conjunct) {
        Some(c) => c.eq_ignore_ascii_case(&index_col.collation),
        None => {
            index_col.collation.is_empty() || index_col.collation.eq_ignore_ascii_case("BINARY")
        }
    }
}

/// Column name of a (possibly COLLATE-wrapped) operand: collation
/// rewriting wraps the COLUMN operand of a comparison in `Expr::Collate`,
/// leaving the column identity unchanged underneath. The collation is
/// recovered from the ORIGINAL conjunct via `conjunct_collation`, so the
/// unwrap loses nothing. Returns `None` for non-column operands
/// (literals, expressions).
fn column_name_of(e: &Expr) -> Option<&str> {
    match e {
        Expr::Column { name, .. } => Some(name.as_str()),
        Expr::Collate { expr, .. } => column_name_of(expr),
        _ => None,
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
