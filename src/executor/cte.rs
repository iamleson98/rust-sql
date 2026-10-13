//! WITH-clause (CTE) materialization.
//!
//! A CTE is computed BEFORE the statement that references it is planned:
//! the rows land in a name → (rows, qualified columns) map that the
//! planner turns into `Plan::CteRows` atoms and `ExecContext::ctes`
//! exposes to subquery planning. The functions take only an
//! `ExecContext`, so the SAME machinery serves a statement's top-level
//! WITH (api.rs) and every NESTED one — a WITH inside a FROM subquery,
//! an expression subquery or a view body is materialized where that
//! statement executes (`exec_select_statement`), layered over the
//! enclosing scope (inner names shadow outer ones).

use std::collections::HashMap;
use std::sync::Arc;

use super::derived::DerivedMeta;
use crate::error::{Error, Result};
use crate::executor::predicate::CompiledExpr;
use crate::planner::plan::Plan;
use crate::planner::Planner;
use crate::sql::ast::Expr;
use crate::sql::ast::{Cte, SelectBody, SelectStatement, SetOp, WithClause};
use crate::types::{CteMaterialization, Row};

use super::{ExecContext, ExecResult};

pub(crate) type CteMap = HashMap<String, CteMaterialization>;

/// A materialized CTE: rows, qualified column names, column types.
type Materialized = (Vec<Row>, Arc<[String]>, Option<Arc<DerivedMeta>>);

/// Plan + execute `select` with `outer_ctes` in scope. The statement's own
/// WITH clause (if any) is materialized first, layered over the outer
/// map. `ctx.ctes` is restored on every exit path, so a nested call never
/// clobbers the enclosing statement's CTE scope.
pub(crate) fn exec_select_with_ctes(
    ctx: &mut ExecContext<'_>,
    select: &SelectStatement,
    outer_ctes: &CteMap,
) -> Result<ExecResult> {
    Ok(exec_select_with_ctes_meta(ctx, select, outer_ctes, None)?.0)
}

/// [`exec_select_with_ctes`], also describing the result set's column
/// types under FROM qualifier `qualifier` (a CTE body: see
/// `derived::derived_meta`).
fn exec_select_with_ctes_meta(
    ctx: &mut ExecContext<'_>,
    select: &SelectStatement,
    outer_ctes: &CteMap,
    qualifier: Option<&str>,
) -> Result<(ExecResult, Option<Arc<DerivedMeta>>)> {
    let catalog = ctx.catalog();
    // WHERE pushdown into single-use CTE bodies (the pushDownWhereTerms
    // analog for CTEs): a conjunct over a CTE referenced exactly once
    // re-homes into the CTE body's WHERE before materialization, so the
    // body's planning sees it (index ranges, rowid lookups). The
    // modified statement clone carries both the reduced outer WHERE and
    // the augmented bodies.
    let pushed_stmt: Option<SelectStatement> = if select.with.as_ref().is_some_and(|w| !w.recursive)
    {
        Planner::new(catalog).push_where_into_cte_bodies(select)
    } else {
        None
    };
    let select_ref: &SelectStatement = pushed_stmt.as_ref().unwrap_or(select);
    // Materialize THIS select's own WITH clause, layered on top of the
    // outer map (inner names shadow outer names).
    let cte_map = match &select_ref.with {
        Some(with) => materialize_ctes(with, outer_ctes, ctx)?,
        None => outer_ctes.clone(),
    };
    let mut planner = Planner::new(catalog);
    planner.set_ctes(cte_map.clone());
    if let Some(f) = ctx.foreign.clone() {
        planner.set_foreign(f);
    }
    let plan = planner.plan_select(select_ref)?;
    let meta = qualifier.and_then(|q| super::derived::derived_meta(&plan, q));
    // Make the CTEs visible to subquery planning inside this statement.
    let saved = ctx.ctes.replace(cte_map);
    let res = (|| {
        // Uncorrelated subquery substitution (same as the general path).
        let plan = if super::plan_has_subqueries(&plan) {
            super::rewrite_plan_subqueries(&plan, ctx)?
        } else {
            plan
        };
        super::execute(&plan, ctx)
    })();
    ctx.ctes = saved;
    Ok((res?, meta))
}

/// Materialize every CTE of `with`, in declaration order (each sees the
/// ones before it), on top of `outer_ctes`. Returns the combined map.
pub(crate) fn materialize_ctes(
    with: &WithClause,
    outer_ctes: &CteMap,
    ctx: &mut ExecContext<'_>,
) -> Result<CteMap> {
    let mut map: CteMap = outer_ctes.clone();
    for cte in &with.ctes {
        let name_lc = cte.name.to_ascii_lowercase();
        let (rows, cols, meta) = if with.recursive {
            materialize_recursive_cte(cte, &map, ctx)?
        } else {
            let (res, meta) = exec_select_with_ctes_meta(ctx, &cte.select, &map, Some(&name_lc))?;
            (res.rows, res.columns, meta)
        };
        let meta = match (&cte.columns, meta) {
            (Some(list), Some(m)) => Some(m.renamed(&name_lc, Some(list))),
            (_, m) => m,
        };
        // Apply the explicit column list (WITH name(a, b) AS ...) — the
        // rename happens at the CTE boundary.
        let cols: Arc<[String]> = match &cte.columns {
            Some(list) if list.len() == cols.len() => list
                .iter()
                .map(|c| format!("{}.{}", cte.name, c))
                .collect::<Vec<String>>()
                .into(),
            Some(list) => {
                return Err(Error::semantic(format!(
                    "CTE {} declares {} columns but its SELECT produces {}",
                    cte.name,
                    list.len(),
                    cols.len()
                )));
            }
            None => {
                // Qualify with the CTE name so `cte.col` references
                // resolve; unqualified refs match by suffix.
                cols.iter()
                    .map(|c| {
                        let suffix = c.rsplit('.').next().unwrap_or(c);
                        format!("{}.{}", cte.name, suffix)
                    })
                    .collect::<Vec<String>>()
                    .into()
            }
        };
        map.insert(name_lc, (Arc::new(rows), cols, meta));
    }
    Ok(map)
}

/// WITH RECURSIVE (SQLite's generateWithRecursiveQuery): the CTE body is
/// `setup UNION [ALL] recursive`. The setup rows seed a QUEUE; each row is
/// taken off the queue, output (after the compound's OFFSET, until its
/// LIMIT), and then the recursive arm runs with the CTE table holding just
/// that row, its results joining the queue (UNION drops rows already seen).
/// The compound's ORDER BY makes the queue a priority queue (ties in
/// insertion order); LIMIT and OFFSET count OUTPUT rows — the recursion
/// stops the moment the LIMIT is reached.
///
/// Without an ORDER BY the queue is FIFO, so rows leave it level by level:
/// a whole level (the frontier) runs through the recursive arm at once —
/// row for row the same output when the arm reads only the CTE (it then
/// emits each frontier row's results in frontier order). The arm is
/// planned ONCE and re-pointed at each new frontier (see
/// `retarget_cte_rows`).
fn materialize_recursive_cte(
    cte: &Cte,
    outer_ctes: &CteMap,
    ctx: &mut ExecContext<'_>,
) -> Result<Materialized> {
    let name_lc = cte.name.to_ascii_lowercase();
    let Some(split) = split_compound_cte(&cte.select, &name_lc)? else {
        // Never recurses: an ordinary CTE.
        let (res, meta) = exec_select_with_ctes_meta(ctx, &cte.select, outer_ctes, Some(&name_lc))?;
        return Ok((res.rows, res.columns, meta));
    };
    let (base, recursive_op, recursive_arms) = (split.setup, split.op, split.arms);
    let sel = &cte.select;
    // computeLimitRegisters: LIMIT first, then (unless it is 0) OFFSET.
    let limit = match &sel.limit {
        Some(e) => super::limit_operand(ctx, e)?,
        None => -1,
    };
    let offset = match &sel.offset {
        Some(e) if limit != 0 => super::limit_operand(ctx, e)?.max(0),
        _ => 0,
    };
    // Setup: executed with the CTE visible but EMPTY. LIMIT 0 runs
    // nothing (the setup's own LIMIT 0 keeps its column names).
    let mut scope = outer_ctes.clone();
    scope.insert(
        name_lc.clone(),
        (
            Arc::new(Vec::new()),
            Arc::from(vec![format!("{}.", cte.name)]),
            None,
        ),
    );
    let base_res = if limit == 0 {
        let mut b = base.clone();
        b.limit = Some(crate::sql::ast::Expr::Literal(
            crate::types::Value::Integer(0),
        ));
        b.offset = None;
        exec_select_with_ctes_meta(ctx, &b, &scope, Some(&name_lc))?
    } else {
        exec_select_with_ctes_meta(ctx, &base, &scope, Some(&name_lc))?
    };
    // The CTE's column types come from its setup (the leftmost arms) —
    // the recursive self-reference and the outer query compare through
    // them.
    let (base_res, meta) = base_res;
    let meta = match (&cte.columns, meta) {
        (Some(list), Some(m)) => Some(m.renamed(&name_lc, Some(list))),
        (_, m) => m,
    };
    let base_cols: Arc<[String]> = base_res.columns;
    // Canonical output columns for the CTE.
    let out_cols: Arc<[String]> = match &cte.columns {
        Some(list) if list.len() == base_cols.len() => list
            .iter()
            .map(|c| format!("{}.{}", cte.name, c))
            .collect::<Vec<String>>()
            .into(),
        Some(list) => {
            return Err(Error::semantic(format!(
                "CTE {} declares {} columns but its SELECT produces {}",
                cte.name,
                list.len(),
                base_cols.len()
            )));
        }
        None => base_cols
            .iter()
            .map(|c| {
                let suffix = c.rsplit('.').next().unwrap_or(c);
                format!("{}.{}", cte.name, suffix)
            })
            .collect::<Vec<String>>()
            .into(),
    };
    if limit == 0 {
        return Ok((Vec::new(), out_cols, meta));
    }
    let order = resolve_queue_order(&sel.order_by, &out_cols)?;
    // Arms that compile to positional expressions run without the planner
    // and executor (see `CompiledArm`) — only when the CTE's columns carry
    // no affinity or collation, so a raw comparison is SQLite's.
    let compiled_arms: Vec<Option<CompiledArm>> = if setup_is_plain(&base) {
        let bare: Vec<String> = out_cols
            .iter()
            .map(|c| {
                c.rsplit_once('.')
                    .map_or(c.as_str(), |(_, n)| n)
                    .to_string()
            })
            .collect();
        recursive_arms
            .iter()
            .map(|a| CompiledArm::compile(a, &name_lc, &bare, ctx.params.len()))
            .collect()
    } else {
        recursive_arms.iter().map(|_| None).collect()
    };
    let union = recursive_op == SetOp::Union;
    // UNION's distinct set (the queue only admits unseen rows).
    // Rows equal under the columns' collations are duplicates.
    let colls: Vec<Option<String>> = meta.as_ref().map_or_else(Vec::new, |m| {
        m.table
            .columns
            .iter()
            .map(|c| Some(c.collation.clone()).filter(|c| !c.eq_ignore_ascii_case("BINARY")))
            .collect()
    });
    let collated = colls.iter().any(|c| c.is_some());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut admit = |r: &Row| -> bool {
        if !union {
            return true;
        }
        if collated {
            let key: Row = r
                .iter()
                .enumerate()
                .map(|(i, v)| match colls.get(i).and_then(|c| c.as_deref()) {
                    Some(c) => crate::plugin::collation_fold_key(c, v),
                    None => v.clone(),
                })
                .collect();
            return seen.insert(format!("{:?}", key));
        }
        seen.insert(format!("{:?}", r))
    };
    let mut out: Vec<Row> = Vec::new();
    let mut skipped = 0i64;
    let mut arm_plans: Vec<Option<ArmPlan>> = recursive_arms.iter().map(|_| None).collect();
    // Output one queue row; `false` once the LIMIT is reached.
    let mut emit = |r: &Row, out: &mut Vec<Row>| -> Result<bool> {
        if skipped < offset {
            skipped += 1;
            return Ok(true);
        }
        if out.len() >= MAX_RECURSIVE_ROWS {
            return Err(Error::semantic(format!(
                "recursive CTE {} exceeded {} rows",
                cte.name, MAX_RECURSIVE_ROWS
            )));
        }
        out.push(r.clone());
        Ok(limit < 0 || (out.len() as i64) < limit)
    };
    let setup: Vec<Row> = base_res.rows.into_iter().filter(|r| admit(r)).collect();
    // Whole-frontier steps reproduce the one-row-at-a-time output order
    // when a single arm reads only the CTE; otherwise only the output
    // SET is the same, which is all that matters without LIMIT / OFFSET.
    let batch = order.is_empty()
        && ((recursive_arms.len() == 1 && simple_recursive_arm(&recursive_arms[0], &name_lc))
            || (limit < 0 && offset == 0));
    if batch {
        let mut frontier = setup;
        while !frontier.is_empty() {
            for r in &frontier {
                if !emit(r, &mut out)? {
                    return Ok((out, out_cols, meta));
                }
            }
            let mut frontier_rows = Some(frontier);
            let mut next_frontier = Vec::new();
            let n_arms = recursive_arms.len();
            for (i, (arm, plan)) in recursive_arms.iter().zip(arm_plans.iter_mut()).enumerate() {
                if let Some(c) = &compiled_arms[i] {
                    let rows = frontier_rows.as_deref().unwrap_or_default();
                    c.run(rows, &ctx.params, &mut |r| {
                        if admit(&r) {
                            next_frontier.push(r);
                        }
                    });
                    continue;
                }
                // The last arm takes the frontier itself.
                let rows = if i + 1 == n_arms {
                    frontier_rows.take().unwrap_or_default()
                } else {
                    frontier_rows.clone().unwrap_or_default()
                };
                let next = run_recursive_arm(
                    ctx, arm, &mut scope, &name_lc, &out_cols, &meta, rows, plan,
                )?;
                next_frontier.extend(next.into_iter().filter(|r| admit(r)));
            }
            frontier = next_frontier;
        }
        return Ok((out, out_cols, meta));
    }
    // One row at a time: FIFO, or ordered by the compound's ORDER BY
    // (ties in insertion order).
    let mut queue = RecursiveQueue::new(order);
    for r in setup {
        queue.push(r);
    }
    while let Some(row) = queue.pop() {
        if !emit(&row, &mut out)? {
            break;
        }
        for (i, (arm, plan)) in recursive_arms.iter().zip(arm_plans.iter_mut()).enumerate() {
            if let Some(c) = &compiled_arms[i] {
                c.run(std::slice::from_ref(&row), &ctx.params, &mut |r| {
                    if admit(&r) {
                        queue.push(r);
                    }
                });
                continue;
            }
            let next = run_recursive_arm(
                ctx,
                arm,
                &mut scope,
                &name_lc,
                &out_cols,
                &meta,
                vec![row.clone()],
                plan,
            )?;
            for r in next {
                if admit(&r) {
                    queue.push(r);
                }
            }
        }
    }
    Ok((out, out_cols, meta))
}

/// A recursive arm that reads only the CTE — `SELECT exprs FROM cte
/// [WHERE a AND b ...]` — compiled to positional expressions over the
/// CTE row, run per row without the planner or executor (SQLite runs this
/// shape as a few VDBE opcodes per row). Built only when the CTE's columns
/// carry no affinity or collation, so the raw comparisons below are
/// exactly the evaluator's; compiled expressions cannot fail, so the
/// conjuncts' order does not matter.
struct CompiledArm {
    filters: Vec<CompiledExpr>,
    /// `None` = `SELECT *` (the row itself).
    project: Option<Vec<CompiledExpr>>,
}

impl CompiledArm {
    fn compile(
        arm: &SelectStatement,
        name_lc: &str,
        cols: &[String],
        params_len: usize,
    ) -> Option<Self> {
        let SelectBody::Simple(s) = &arm.body else {
            return None;
        };
        let alias = match &s.from {
            Some(crate::sql::ast::TableExpression::Table {
                name,
                schema: None,
                alias,
                indexed: None,
            }) if name.eq_ignore_ascii_case(name_lc) => alias.clone(),
            _ => return None,
        };
        if !s.group_by.is_empty() || s.having.is_some() || s.distinct || !s.window.is_empty() {
            return None;
        }
        let qual = alias.unwrap_or_else(|| name_lc.to_string());
        let compile = |e: &Expr| compile_cte_expr(e, &qual, cols, params_len);
        let mut filters = Vec::new();
        if let Some(w) = &s.where_clause {
            let mut stack = vec![w];
            while let Some(e) = stack.pop() {
                match e {
                    Expr::Binary {
                        op: crate::sql::ast::BinaryOp::And,
                        left,
                        right,
                    } => {
                        stack.push(right);
                        stack.push(left);
                    }
                    other => filters.push(compile(other)?),
                }
            }
        }
        let project = match s.columns.as_slice() {
            [crate::sql::ast::ResultColumn::Star] => None,
            items => {
                let mut out = Vec::with_capacity(items.len());
                for c in items {
                    match c {
                        crate::sql::ast::ResultColumn::Expr { expr, .. } => {
                            out.push(compile(expr)?)
                        }
                        _ => return None,
                    }
                }
                if out.len() != cols.len() {
                    return None;
                }
                Some(out)
            }
        };
        Some(Self { filters, project })
    }

    fn run(&self, rows: &[Row], params: &[crate::types::Value], sink: &mut dyn FnMut(Row)) {
        for row in rows {
            if !self.filters.iter().all(|f| f.eval(row, params).is_truthy()) {
                continue;
            }
            sink(match &self.project {
                Some(p) => p.iter().map(|e| e.eval(row, params)).collect(),
                None => row.clone(),
            });
        }
    }
}

/// Positional compilation over the CTE row: literals, parameters, the
/// CTE's columns (bare or qualified by the CTE / its alias), unary and
/// binary operators (comparisons included — the columns carry no
/// affinity, see `CompiledArm`). AND / OR stay out (value-context
/// short-circuit rules).
fn compile_cte_expr(
    e: &Expr,
    qual: &str,
    cols: &[String],
    params_len: usize,
) -> Option<CompiledExpr> {
    use crate::sql::ast::BinaryOp;
    Some(match e {
        Expr::Literal(v) => CompiledExpr::Literal(v.clone()),
        Expr::Column { table, name } => {
            if table
                .as_deref()
                .is_some_and(|t| !t.eq_ignore_ascii_case(qual))
            {
                return None;
            }
            CompiledExpr::Col(cols.iter().position(|c| c.eq_ignore_ascii_case(name))?)
        }
        Expr::Parameter(_) => crate::executor::predicate::compile_expr(e, cols, params_len)?,
        Expr::Unary { op, expr } => CompiledExpr::Unary(
            *op,
            Box::new(compile_cte_expr(expr, qual, cols, params_len)?),
        ),
        Expr::Binary { op, left, right } => {
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                return None;
            }
            CompiledExpr::Binary(
                *op,
                Box::new(compile_cte_expr(left, qual, cols, params_len)?),
                Box::new(compile_cte_expr(right, qual, cols, params_len)?),
            )
        }
        _ => return None,
    })
}

/// Every setup arm's result column is an expression with NO affinity and
/// no collation (a literal, arithmetic, a function call — not a column,
/// a CAST or a COLLATE): the CTE's columns then compare raw.
fn setup_is_plain(setup: &SelectStatement) -> bool {
    fn body_plain(b: &SelectBody) -> bool {
        match b {
            SelectBody::Simple(s) => s.columns.iter().all(|c| match c {
                crate::sql::ast::ResultColumn::Expr { expr, .. } => {
                    crate::executor::expr::expr_affinity_with(expr, &|_| {
                        Some(crate::types::Affinity::Blob)
                    })
                    .is_none()
                        && crate::executor::expr::explicit_collation(expr).is_none()
                        && !expr_reads_column(expr)
                }
                _ => false,
            }),
            SelectBody::Binary { left, right, .. } => body_plain(left) && body_plain(right),
        }
    }
    body_plain(&setup.body)
}

/// Whether an expression references any column (a column-derived value
/// can carry a declared collation through arithmetic-free shapes; stay
/// conservative and keep such setups on the general path).
fn expr_reads_column(e: &Expr) -> bool {
    let mut refs = Vec::new();
    crate::executor::collect_expr_refs(e, &mut refs);
    !refs.is_empty()
}

/// Output-row guard for a recursion that never ends (SQLite would run
/// until memory runs out).
const MAX_RECURSIVE_ROWS: usize = 10_000_000;

/// The recursive arm's plan, kept across iterations.
struct ArmPlan {
    plan: Plan,
    /// The frontier the plan's `CteRows` currently read.
    current: Arc<Vec<Row>>,
    /// Frontier size the plan was costed with.
    planned_len: usize,
    /// The plan reads the CTE through `CteRows` nodes that can be
    /// re-pointed; false = re-plan every iteration.
    reusable: bool,
    has_subqueries: bool,
}

/// Run the recursive arm with the CTE table holding `frontier`.
fn run_recursive_arm(
    ctx: &mut ExecContext<'_>,
    arm: &SelectStatement,
    scope: &mut CteMap,
    name_lc: &str,
    cols: &Arc<[String]>,
    meta: &Option<Arc<DerivedMeta>>,
    frontier: Vec<Row>,
    cache: &mut Option<ArmPlan>,
) -> Result<Vec<Row>> {
    let f = Arc::new(frontier);
    scope.insert(
        name_lc.to_string(),
        (Arc::clone(&f), Arc::clone(cols), meta.clone()),
    );
    let reused = match cache.as_mut() {
        // Re-plan when the frontier outgrows the size the plan was costed
        // for (join order and algorithm follow the CTE's row count).
        Some(c) if c.reusable && f.len() <= c.planned_len.saturating_mul(4) + 16 => {
            let old = Arc::clone(&c.current);
            if retarget_cte_rows(&mut c.plan, &old, &f) > 0 {
                c.current = Arc::clone(&f);
                true
            } else {
                false
            }
        }
        _ => false,
    };
    if !reused {
        let mut planner = Planner::new(ctx.catalog());
        planner.set_ctes(scope.clone());
        if let Some(fr) = ctx.foreign.clone() {
            planner.set_foreign(fr);
        }
        let mut plan = planner.plan_select(arm)?;
        let refs = retarget_cte_rows(&mut plan, &f, &f);
        let has_subqueries = super::plan_has_subqueries(&plan);
        *cache = Some(ArmPlan {
            plan,
            current: Arc::clone(&f),
            planned_len: f.len(),
            reusable: refs > 0,
            has_subqueries,
        });
    }
    let c = cache.as_ref().expect("arm plan just set");
    // The CTE scope is visible to subquery planning while the arm runs.
    let saved = ctx.ctes.replace(std::mem::take(scope));
    let res = if c.has_subqueries {
        super::rewrite_plan_subqueries(&c.plan, ctx).and_then(|p| super::execute(&p, ctx))
    } else {
        super::execute(&c.plan, ctx)
    };
    *scope = ctx.ctes.take().unwrap_or_default();
    ctx.ctes = saved;
    Ok(res?.rows)
}

/// Point every `CteRows` node reading `old` (the previous frontier) at
/// `new`; returns how many nodes it re-pointed (`old == new` counts them).
fn retarget_cte_rows(plan: &mut Plan, old: &Arc<Vec<Row>>, new: &Arc<Vec<Row>>) -> usize {
    match plan {
        Plan::CteRows { rows, .. } => {
            if Arc::ptr_eq(rows, old) {
                *rows = Arc::clone(new);
                1
            } else {
                0
            }
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Aggregate { input, .. }
        | Plan::Window { input, .. }
        | Plan::Distinct { input } => retarget_cte_rows(input, old, new),
        Plan::Join { left, right, .. }
        | Plan::Union { left, right, .. }
        | Plan::Intersect { left, right }
        | Plan::Except { left, right } => {
            retarget_cte_rows(left, old, new) + retarget_cte_rows(right, old, new)
        }
        Plan::IndexNestedLoopJoin { outer, .. } => retarget_cte_rows(outer, old, new),
        Plan::Subquery { plan, .. } => retarget_cte_rows(plan, old, new),
        Plan::Insert { source, .. } | Plan::Update { source, .. } | Plan::Delete { source, .. } => {
            retarget_cte_rows(source, old, new)
        }
        _ => 0,
    }
}

/// The arm reads ONLY the CTE (one FROM item, no grouping): its results
/// over a whole frontier come out frontier row by frontier row.
fn simple_recursive_arm(arm: &SelectStatement, name_lc: &str) -> bool {
    let SelectBody::Simple(s) = &arm.body else {
        return false;
    };
    matches!(
        &s.from,
        Some(crate::sql::ast::TableExpression::Table { name, schema: None, .. })
            if name.eq_ignore_ascii_case(name_lc)
    ) && s.group_by.is_empty()
        && s.having.is_none()
        && !s.distinct
}

/// One ORDER BY term of the recursive queue: the output column, the
/// direction, NULLS placement and an explicit collation.
struct QueueTerm {
    col: usize,
    desc: bool,
    nulls_first: bool,
    coll: Option<Arc<dyn crate::plugin::Collation>>,
}

/// Resolve the compound's ORDER BY against the CTE's output columns —
/// a compound ORDER BY term must name (or number) a result column.
fn resolve_queue_order(
    terms: &[crate::sql::ast::OrderTerm],
    cols: &[String],
) -> Result<Vec<QueueTerm>> {
    use crate::sql::ast::{Expr, NullsOrder, Order};
    let mut out = Vec::with_capacity(terms.len());
    for (i, t) in terms.iter().enumerate() {
        let (expr, coll) = match &t.expr {
            Expr::Collate { expr, collation } => (expr.as_ref(), Some(collation.clone())),
            e => (e, None),
        };
        let col = match expr {
            Expr::Literal(crate::types::Value::Integer(k))
                if *k >= 1 && (*k as usize) <= cols.len() =>
            {
                Some(*k as usize - 1)
            }
            Expr::Column { table, name } => cols.iter().position(|c| {
                let (q, n) = c.rsplit_once('.').unwrap_or(("", c));
                n.eq_ignore_ascii_case(name)
                    && table.as_deref().map_or(true, |t| t.eq_ignore_ascii_case(q))
            }),
            _ => None,
        };
        let Some(col) = col else {
            return Err(Error::semantic(format!(
                "{} ORDER BY term does not match any column in the result set",
                crate::planner::sqlite_ordinal(i as i64 + 1)
            )));
        };
        let coll = match coll {
            None => None,
            Some(n) if n.eq_ignore_ascii_case("binary") => None,
            Some(n) => match crate::plugin::lookup_collation(&n) {
                Some(c) => Some(c),
                None => {
                    return Err(Error::semantic(format!("no such collation sequence: {n}")));
                }
            },
        };
        let desc = matches!(t.order, Order::Desc);
        let nulls_first = match t.nulls {
            NullsOrder::First => true,
            NullsOrder::Last => false,
            NullsOrder::Default => !desc,
        };
        out.push(QueueTerm {
            col,
            desc,
            nulls_first,
            coll,
        });
    }
    Ok(out)
}

/// The recursion queue: FIFO without an ORDER BY, else a min-heap on the
/// ORDER BY terms with ties broken by insertion order.
struct RecursiveQueue {
    order: Vec<QueueTerm>,
    fifo: std::collections::VecDeque<Row>,
    heap: Vec<(u64, Row)>,
    seq: u64,
}

impl RecursiveQueue {
    fn new(order: Vec<QueueTerm>) -> Self {
        Self {
            order,
            fifo: std::collections::VecDeque::new(),
            heap: Vec::new(),
            seq: 0,
        }
    }

    fn less(&self, a: &(u64, Row), b: &(u64, Row)) -> bool {
        use std::cmp::Ordering;
        for t in &self.order {
            let (x, y) = (&a.1[t.col], &b.1[t.col]);
            let ord = match (x.is_null(), y.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if t.nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if t.nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => {
                    let o = match (&t.coll, x, y) {
                        (Some(c), crate::types::Value::Text(p), crate::types::Value::Text(q)) => {
                            c.compare(p.as_str(), q.as_str())
                        }
                        _ => x.cmp(y),
                    };
                    if t.desc {
                        o.reverse()
                    } else {
                        o
                    }
                }
            };
            if ord != Ordering::Equal {
                return ord == Ordering::Less;
            }
        }
        a.0 < b.0
    }

    fn push(&mut self, r: Row) {
        if self.order.is_empty() {
            self.fifo.push_back(r);
            return;
        }
        self.seq += 1;
        self.heap.push((self.seq, r));
        let mut i = self.heap.len() - 1;
        while i > 0 {
            let p = (i - 1) / 2;
            if self.less(&self.heap[i], &self.heap[p]) {
                self.heap.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
    }

    fn pop(&mut self) -> Option<Row> {
        if self.order.is_empty() {
            return self.fifo.pop_front();
        }
        if self.heap.is_empty() {
            return None;
        }
        let last = self.heap.len() - 1;
        self.heap.swap(0, last);
        let top = self.heap.pop().map(|(_, r)| r);
        let n = self.heap.len();
        let mut i = 0;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut m = i;
            if l < n && self.less(&self.heap[l], &self.heap[m]) {
                m = l;
            }
            if r < n && self.less(&self.heap[r], &self.heap[m]) {
                m = r;
            }
            if m == i {
                break;
            }
            self.heap.swap(i, m);
            i = m;
        }
        top
    }
}

/// A recursive CTE's compound body split as SQLite's
/// generateWithRecursiveQuery reads it: the SETUP arms (a prefix that does
/// not reference the CTE, kept as one compound), the operator joining the
/// first recursive arm, and the RECURSIVE arms (every arm from the first
/// one that reads the CTE to the end). The compound's ORDER BY / LIMIT /
/// OFFSET belong to the recursion, not to any arm.
struct RecursiveSplit {
    setup: SelectStatement,
    op: SetOp,
    arms: Vec<SelectStatement>,
}

/// `None` when no arm reads the CTE: a WITH RECURSIVE CTE that never
/// recurses is an ordinary one.
fn split_compound_cte(sel: &SelectStatement, name_lc: &str) -> Result<Option<RecursiveSplit>> {
    fn stmt(body: SelectBody) -> SelectStatement {
        SelectStatement {
            with: None,
            body,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        }
    }
    // Flatten the left-deep compound: arms in order, each with the
    // operator that joins it to the arms before it.
    fn flatten(body: &SelectBody, out: &mut Vec<(Option<SetOp>, SelectBody)>) {
        match body {
            SelectBody::Binary { op, left, right } => {
                flatten(left, out);
                out.push((Some(*op), (**right).clone()));
            }
            simple => out.push((None, simple.clone())),
        }
    }
    let mut arms = Vec::new();
    flatten(&sel.body, &mut arms);
    let Some(first_rec) = arms.iter().position(|(_, b)| body_reads_table(b, name_lc)) else {
        return Ok(None);
    };
    let circular = || Error::semantic(format!("circular reference: {name_lc}"));
    if first_rec == 0
        || arms[first_rec..]
            .iter()
            .any(|(_, b)| !body_reads_table(b, name_lc))
    {
        return Err(circular());
    }
    let op = match arms[first_rec].0 {
        Some(op @ (SetOp::Union | SetOp::UnionAll)) => op,
        _ => {
            return Err(Error::semantic(
                "WITH RECURSIVE requires UNION or UNION ALL as the compound operator",
            ))
        }
    };
    let rec: Vec<SelectStatement> = arms.drain(first_rec..).map(|(_, b)| stmt(b)).collect();
    let mut setup_body: Option<SelectBody> = None;
    for (op, b) in arms {
        setup_body = Some(match (setup_body, op) {
            (Some(left), Some(op)) => SelectBody::Binary {
                op,
                left: Box::new(left),
                right: Box::new(b),
            },
            _ => b,
        });
    }
    Ok(Some(RecursiveSplit {
        setup: stmt(setup_body.expect("first_rec > 0")),
        op,
        arms: rec,
    }))
}

/// Whether an arm's FROM reads the table `name_lc` (directly, in a join,
/// or inside a FROM subquery).
fn body_reads_table(body: &SelectBody, name_lc: &str) -> bool {
    fn te_reads(t: &crate::sql::ast::TableExpression, name_lc: &str) -> bool {
        use crate::sql::ast::TableExpression as T;
        match t {
            T::Table {
                name, schema: None, ..
            } => name.eq_ignore_ascii_case(name_lc),
            T::Join { left, right, .. } => te_reads(left, name_lc) || te_reads(right, name_lc),
            T::Subquery { select, .. } => body_reads_table(&select.body, name_lc),
            _ => false,
        }
    }
    match body {
        SelectBody::Simple(s) => s.from.as_ref().is_some_and(|f| te_reads(f, name_lc)),
        SelectBody::Binary { left, right, .. } => {
            body_reads_table(left, name_lc) || body_reads_table(right, name_lc)
        }
    }
}
