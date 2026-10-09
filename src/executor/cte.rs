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

use crate::error::{Error, Result};
use crate::planner::Planner;
use crate::sql::ast::{Cte, SelectBody, SelectStatement, SetOp, WithClause};
use crate::types::{CteMaterialization, Row};

use super::{ExecContext, ExecResult};

pub(crate) type CteMap = HashMap<String, CteMaterialization>;

/// Plan + execute `select` with `outer_ctes` in scope. The statement's own
/// WITH clause (if any) is materialized first, layered over the outer
/// map. `ctx.ctes` is restored on every exit path, so a nested call never
/// clobbers the enclosing statement's CTE scope.
pub(crate) fn exec_select_with_ctes(
    ctx: &mut ExecContext<'_>,
    select: &SelectStatement,
    outer_ctes: &CteMap,
) -> Result<ExecResult> {
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
    res
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
        let (rows, cols) = if with.recursive {
            materialize_recursive_cte(cte, &map, ctx)?
        } else {
            let res = exec_select_with_ctes(ctx, &cte.select, &map)?;
            (res.rows, res.columns)
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
        map.insert(name_lc, (Arc::new(rows), cols));
    }
    Ok(map)
}

/// WITH RECURSIVE: the CTE body is `base UNION [ALL] recursive`. The
/// base arm executes once; the recursive arm (which references the CTE
/// by name) executes repeatedly, each time seeing ALL rows accumulated
/// so far, until it produces no new rows. UNION dedups against the
/// accumulated set; UNION ALL appends everything. A hard iteration cap
/// guards against non-terminating recursions (SQLite errors too).
fn materialize_recursive_cte(
    cte: &Cte,
    outer_ctes: &CteMap,
    ctx: &mut ExecContext<'_>,
) -> Result<(Vec<Row>, Arc<[String]>)> {
    let name_lc = cte.name.to_ascii_lowercase();
    // Split the compound body: the LAST UNION [ALL] arm is the
    // recursive one; everything before it is the base. (SQLite's rule:
    // exactly one recursive reference, in the arm after the UNION.)
    let (base, recursive_op, recursive_arm) = split_compound_cte(&cte.select)?;
    // Base: execute with the CTE visible but EMPTY (a recursive
    // reference in the base arm is an error in SQLite; empty keeps it
    // simple and correct for well-formed queries).
    let mut scope = outer_ctes.clone();
    scope.insert(
        name_lc.clone(),
        (
            Arc::new(Vec::new()),
            Arc::from(vec![format!("{}.", cte.name)]),
        ),
    );
    let base_res = exec_select_with_ctes(ctx, &base, &scope)?;
    let mut rows: Vec<Row> = base_res.rows;
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
    // Dedup set for UNION semantics.
    let mut seen: std::collections::HashSet<String> =
        rows.iter().map(|r| format!("{:?}", r)).collect();
    // Iterate the recursive arm with QUEUE semantics (SQLite's model):
    // each iteration's arm sees ONLY the rows produced by the PREVIOUS
    // iteration (the frontier), not the full accumulation — otherwise
    // UNION ALL recursions never terminate (every iteration re-derives
    // rows that are "new" again as duplicates).
    let mut frontier: Vec<Row> = rows.clone();
    const MAX_ITERS: usize = 1_000_000;
    for _ in 0..MAX_ITERS {
        if rows.len() > 10_000_000 {
            return Err(Error::semantic(format!(
                "recursive CTE {} exceeded 10,000,000 rows",
                cte.name
            )));
        }
        if frontier.is_empty() {
            break;
        }
        let mut scope = outer_ctes.clone();
        scope.insert(name_lc.clone(), (Arc::new(frontier), out_cols.clone()));
        let new_res = exec_select_with_ctes(ctx, &recursive_arm, &scope)?;
        let mut next_frontier: Vec<Row> = Vec::with_capacity(new_res.rows.len());
        for r in new_res.rows {
            if recursive_op == SetOp::Union {
                let key = format!("{:?}", r);
                if seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
            }
            next_frontier.push(r.clone());
            rows.push(r);
        }
        frontier = next_frontier;
    }
    Ok((rows, out_cols))
}

/// Split a recursive CTE's compound SELECT into (base, set-op, recursive arm).
/// The body must be `... UNION [ALL] <recursive>`; more than two arms or a
/// non-UNION top-level operator is rejected (SQLite requires the same shape).
fn split_compound_cte(sel: &SelectStatement) -> Result<(SelectStatement, SetOp, SelectStatement)> {
    // Rebuild a plain SelectStatement from a SelectBody by cloning the
    // statement shell with the body swapped.
    fn with_body(s: &SelectStatement, body: SelectBody) -> SelectStatement {
        let mut out = s.clone();
        out.body = body;
        out.with = None; // the outer WITH doesn't apply to inner arms
        out
    }
    match &sel.body {
        SelectBody::Binary { op, left, right } => match op {
            SetOp::Union | SetOp::UnionAll => Ok((
                with_body(sel, (**left).clone()),
                *op,
                with_body(sel, (**right).clone()),
            )),
            _ => Err(Error::semantic(
                "WITH RECURSIVE requires UNION or UNION ALL as the compound operator",
            )),
        },
        SelectBody::Simple(_) => Err(Error::semantic(
            "WITH RECURSIVE requires a compound SELECT (base UNION [ALL] recursive)",
        )),
    }
}
