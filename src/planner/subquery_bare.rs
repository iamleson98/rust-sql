//! Correlated outer references inside subqueries of an AGGREGATE query
//! (SQLite's bare-column semantics through the subquery scope boundary).
//!
//! `collect_bare_refs` treats `Expr::Subquery` as a scope boundary —
//! correct for the DIRECT projection path (the subquery's own machinery
//! resolves its refs through the runtime outer-scope frames), but under
//! GROUP BY the frame the subquery ultimately binds against is the
//! AGGREGATE OUTPUT row ([group keys..., __agg_N...]), which carries no
//! bare-column values. A correlated reference into a non-grouped,
//! non-aggregated outer column silently evaluated to NULL:
//! `SELECT (SELECT y) FROM t3 JOIN t4 ON x=y GROUP BY x` returned all
//! NULLs (window1 76.4/76.5 — the LEFT-JOIN churn shape).
//!
//! The fix mirrors SQLite: the outer query's bare-column machinery also
//! covers references that LEAK OUT of select-list / HAVING / ORDER BY
//! subqueries. Collection (`subquery_outer_column_refs`) and the
//! rewrite (`rewrite_subquery_outer_refs`) share one scope model, the
//! same one the runtime classifier `subquery_is_correlated` uses: a
//! reference is LOCAL to a subquery when its qualifier names one of the
//! subquery's own sources (pooled across its nested subqueries) or its
//! bare name matches a source column or a visible CTE column. Anything
//! else binds INTO THIS query: collected as a `func: "bare"`
//! pseudo-aggregate slot (build_full_agg_set) and rewritten — inside
//! the subquery tree, to the slot's `__agg_N` output column — so the
//! runtime frame stack resolves it against the Aggregate's output.

use crate::planner::expr_eq::expr_eq;
use crate::planner::plan::AggExpr;
use crate::planner::Planner;
use crate::schema::Table;
use crate::sql::ast::{
    Expr, InSource, JoinConstraint, ResultColumn, SelectBody, SelectStatement, TableExpression,
    WindowSpec,
};
use std::collections::HashSet;
use std::sync::Arc;

/// A subquery atom's POOLED source scope: every table/alias its own
/// FROM introduces, across join trees and nested subqueries (matching
/// `crate::executor::collect_select_sources`). Carried UNCHANGED into
/// nested subquery atoms during the rewrite: a reference that binds at
/// an intermediate level is in the pooled set (left for the runtime
/// frame stack), and one that binds nowhere inside binds here.
struct SubqueryScope {
    quals: HashSet<String>,
    tables: Vec<Arc<Table>>,
}

impl SubqueryScope {
    fn of(p: &Planner<'_>, sel: &SelectStatement) -> Self {
        let mut quals = HashSet::new();
        let mut tables = Vec::new();
        crate::executor::collect_select_sources(sel, p.catalog, &mut quals, &mut tables);
        Self { quals, tables }
    }

    /// Does the reference (as written) resolve inside the subquery's
    /// own scope? Mirrors `subquery_is_correlated`'s checks exactly.
    fn is_local(&self, p: &Planner<'_>, table: &Option<String>, name: &str) -> bool {
        match table {
            Some(q) => self.quals.contains(&q.to_ascii_lowercase()),
            None => {
                let known_table = self.tables.iter().any(|t| {
                    t.find_column(name).is_some()
                        || (crate::planner::is_rowid_spelling(name) && !t.without_rowid)
                });
                known_table || p.cte_column_in_scope(name)
            }
        }
    }
}

impl Planner<'_> {
    /// Is `name` a column of any CTE visible to a subquery of this
    /// statement (its own CTEs plus the outer scope's)?
    pub(crate) fn cte_column_in_scope(&self, name: &str) -> bool {
        let check = |cols: &Arc<[String]>| {
            cols.iter()
                .any(|c| c.rsplit('.').next().unwrap_or(c).eq_ignore_ascii_case(name))
        };
        if self.ctes.values().any(|c| check(&c.1)) {
            return true;
        }
        self.outer_ctes
            .as_ref()
            .map(|m| m.values().any(|c| check(&c.1)))
            .unwrap_or(false)
    }

    /// The column references of `sel` that do NOT resolve inside the
    /// subquery's own scope — the correlated references that bind into
    /// THIS query. Returned as the `Expr::Column` spellings as written
    /// (the bare-slot rewrite keys off exactly these forms).
    pub(crate) fn subquery_outer_column_refs(&self, sel: &SelectStatement) -> Vec<Expr> {
        let scope = SubqueryScope::of(self, sel);
        let mut out: Vec<Expr> = Vec::new();
        for (qual, name) in crate::executor::collect_select_refs(sel) {
            if scope.is_local(self, &qual, &name) {
                continue;
            }
            let e = Expr::Column { table: qual, name };
            if !out.iter().any(|x| expr_eq(x, &e)) {
                out.push(e);
            }
        }
        out
    }

    /// Collect the correlated bare-column references that leak out of
    /// subquery atoms anywhere inside `e` (a select-list / HAVING /
    /// ORDER BY expression of an aggregate query). Same group-term
    /// cutoff as `collect_bare_refs`: a reference that display-matches
    /// a GROUP BY term resolves through the group-key output column at
    /// runtime — no bare slot needed.
    pub(crate) fn collect_subquery_bare_refs(
        &self,
        e: &Expr,
        group_by: &[Expr],
        out: &mut Vec<Expr>,
    ) {
        walk_subquery_atoms(e, &mut |sel| {
            for cand in self.subquery_outer_column_refs(sel) {
                let is_group_term = group_by.iter().any(|g| {
                    let inner = matches!(g, Expr::Collate { expr, .. } if expr_eq(expr, &cand));
                    inner || expr_eq(g, &cand)
                });
                if is_group_term {
                    continue;
                }
                if !out.iter().any(|x| expr_eq(x, &cand)) {
                    out.push(cand);
                }
            }
        });
    }

    /// Rewrite correlated outer references INSIDE the subquery atoms of
    /// `e` to the bare pseudo-aggregate output columns (`__agg_N`), so
    /// the runtime frame lookup binds them against the Aggregate's
    /// output row. Runs BEFORE `rewrite_aggregates_and_groups` (which
    /// does not descend into subqueries); the two rewrites touch
    /// disjoint parts of the tree.
    pub(crate) fn rewrite_subquery_outer_refs(
        &self,
        e: &Expr,
        aggregates: &[AggExpr],
        group_by: &[Expr],
    ) -> Expr {
        rewrite_expr(e, self, None, aggregates, group_by)
    }
}

/// Visit every subquery atom (`Expr::Subquery`, `Expr::Exists`,
/// `InSource::Subquery`) at any depth of `e`, WITHOUT descending into
/// the subqueries' own bodies — the per-atom scope check pools nested
/// sources, so one visit covers the whole subtree's outer references.
fn walk_subquery_atoms(e: &Expr, f: &mut impl FnMut(&SelectStatement)) {
    match e {
        Expr::Subquery(sel) | Expr::Exists(sel) => f(sel),
        Expr::In { expr, source, .. } => {
            walk_subquery_atoms(expr, f);
            if let InSource::Subquery(sel) = source {
                f(sel);
            }
        }
        Expr::Binary { left, right, .. } => {
            walk_subquery_atoms(left, f);
            walk_subquery_atoms(right, f);
        }
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => walk_subquery_atoms(expr, f),
        Expr::Between {
            expr, low, high, ..
        } => {
            walk_subquery_atoms(expr, f);
            walk_subquery_atoms(low, f);
            walk_subquery_atoms(high, f);
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            walk_subquery_atoms(expr, f);
            walk_subquery_atoms(pattern, f);
            if let Some(x) = escape {
                walk_subquery_atoms(x, f);
            }
        }
        Expr::Is { left, right, .. } => {
            walk_subquery_atoms(left, f);
            walk_subquery_atoms(right, f);
        }
        Expr::Function {
            name,
            args,
            filter,
            over,
            ..
        } => {
            // Aggregate and window calls own their arguments (see
            // collect_bare_refs' boundary): an aggregate's args evaluate
            // per INPUT row — the raw row's columns bind directly — and
            // the outer rewrite display-matches the collected arg
            // verbatim; rewriting inside it breaks the match (the max()
            // arg subquery case: the unmatched call then evaluated as a
            // scalar, "no such function: max"). Window calls go through
            // their own collection/rewrite pair.
            let is_agg = over.is_none()
                && crate::planner::is_aggregate_call(&name.to_ascii_lowercase(), args.len());
            if !is_agg {
                for a in args {
                    walk_subquery_atoms(a, f);
                }
                if let Some(fl) = filter {
                    walk_subquery_atoms(fl, f);
                }
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                walk_subquery_atoms(o, f);
            }
            for (c, v) in whens {
                walk_subquery_atoms(c, f);
                walk_subquery_atoms(v, f);
            }
            if let Some(x) = else_ {
                walk_subquery_atoms(x, f);
            }
        }
        Expr::Row(items) => {
            for i in items {
                walk_subquery_atoms(i, f);
            }
        }
        Expr::Raise {
            message: Some(m), ..
        } => walk_subquery_atoms(m, f),
        _ => {}
    }
}

/// The bare pseudo-aggregate slot (if any) a correlated outer reference
/// maps to — display-matched against the collected bare args, exactly
/// like the direct-reference arm of `rewrite_aggregates_and_groups`.
fn bare_slot_for(e: &Expr, aggregates: &[AggExpr]) -> Option<usize> {
    for (i, agg) in aggregates.iter().enumerate() {
        if agg.func == "bare" {
            if let Some(arg) = &agg.arg {
                if expr_eq(arg, e) {
                    return Some(i);
                }
            }
        }
    }
    None
}

/// The unified rewriter. `scope`-less (top) mode: look for subquery
/// atoms, fresh scope per atom, outer-tree columns untouched.
/// `scope`-ful (inside a subquery) mode: rewrite matching column
/// references, keep the SAME scope through nested atoms (the pooled
/// model — see the module doc).
fn rewrite_expr(
    e: &Expr,
    p: &Planner<'_>,
    scope: Option<&SubqueryScope>,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) -> Expr {
    // Column rewriting only applies INSIDE a subquery atom.
    if let (Some(scope), Expr::Column { .. }) = (scope, e) {
        let Expr::Column { table, name } = e else {
            unreachable!()
        };
        if !scope.is_local(p, table, name) {
            if let Some(i) = bare_slot_for(e, aggregates) {
                // Group-term cutoff mirrors the collector: a group-key
                // display match resolves through the group output
                // column at runtime.
                let is_group_term = group_by.iter().any(|g| {
                    let inner = matches!(g, Expr::Collate { expr, .. } if expr_eq(expr, e));
                    inner || expr_eq(g, e)
                });
                if !is_group_term {
                    return Expr::Column {
                        table: None,
                        name: format!("__agg_{}", i),
                    };
                }
            }
        }
        return e.clone();
    }
    match e {
        // Subquery atoms: fresh scope at the top level; the carried
        // scope inside (nested atoms share the outer atom's pooled
        // scope by design).
        Expr::Subquery(sel) => {
            let new_sel = match scope {
                None => rewrite_sel(sel, p, None, aggregates, group_by),
                Some(s) => rewrite_sel(sel, p, Some(s), aggregates, group_by),
            };
            Expr::Subquery(Box::new(new_sel))
        }
        Expr::Exists(sel) => {
            let new_sel = match scope {
                None => rewrite_sel(sel, p, None, aggregates, group_by),
                Some(s) => rewrite_sel(sel, p, Some(s), aggregates, group_by),
            };
            Expr::Exists(Box::new(new_sel))
        }
        Expr::In {
            expr,
            source,
            negated,
        } => {
            let source = match source {
                InSource::Subquery(sel) => {
                    let new_sel = match scope {
                        None => rewrite_sel(sel, p, None, aggregates, group_by),
                        Some(s) => rewrite_sel(sel, p, Some(s), aggregates, group_by),
                    };
                    InSource::Subquery(Box::new(new_sel))
                }
                other => other.clone(),
            };
            Expr::In {
                expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
                source,
                negated: *negated,
            }
        }
        Expr::Binary {
            op, left, right, ..
        } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_expr(left, p, scope, aggregates, group_by)),
            right: Box::new(rewrite_expr(right, p, scope, aggregates, group_by)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
            low: Box::new(rewrite_expr(low, p, scope, aggregates, group_by)),
            high: Box::new(rewrite_expr(high, p, scope, aggregates, group_by)),
            negated: *negated,
        },
        Expr::Like {
            op,
            expr,
            pattern,
            escape,
            negated,
        } => Expr::Like {
            op: *op,
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
            pattern: Box::new(rewrite_expr(pattern, p, scope, aggregates, group_by)),
            escape: escape
                .as_ref()
                .map(|x| Box::new(rewrite_expr(x, p, scope, aggregates, group_by))),
            negated: *negated,
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
            negated: *negated,
        },
        Expr::Is {
            left,
            right,
            negated,
        } => Expr::Is {
            left: Box::new(rewrite_expr(left, p, scope, aggregates, group_by)),
            right: Box::new(rewrite_expr(right, p, scope, aggregates, group_by)),
            negated: *negated,
        },
        Expr::Function {
            name,
            distinct,
            args,
            filter,
            over,
            order_by,
        } => {
            // TOP MODE BOUNDARY: aggregate and window calls own their
            // arguments — verbatim (see walk_subquery_atoms). Inside a
            // subquery (scope = Some) the pooled-scope model applies and
            // nested function calls are ordinary expressions.
            if scope.is_none() {
                let is_agg = over.is_none()
                    && crate::planner::is_aggregate_call(&name.to_ascii_lowercase(), args.len());
                let is_window = over.is_some();
                if is_agg || is_window {
                    return e.clone();
                }
            }
            Expr::Function {
                name: name.clone(),
                distinct: *distinct,
                args: args
                    .iter()
                    .map(|a| rewrite_expr(a, p, scope, aggregates, group_by))
                    .collect(),
                filter: filter
                    .as_ref()
                    .map(|f| Box::new(rewrite_expr(f, p, scope, aggregates, group_by))),
                order_by: order_by
                    .iter()
                    .map(|t| {
                        let mut t = t.clone();
                        t.expr = rewrite_expr(&t.expr, p, scope, aggregates, group_by);
                        t
                    })
                    .collect(),
                over: over.as_ref().map(|w| {
                    Box::new(match w.as_ref() {
                        // A NAMED window resolves against the subquery's own
                        // WINDOW clause — its definition's expressions are
                        // rewritten through that clause below (the body
                        // rewrite covers `SimpleSelect::window`).
                        WindowSpec::Named(n) => WindowSpec::Named(n.clone()),
                        WindowSpec::Inline(def) => WindowSpec::Inline(Box::new({
                            let mut d = def.as_ref().clone();
                            for e in d.partition_by.iter_mut() {
                                *e = rewrite_expr(e, p, scope, aggregates, group_by);
                            }
                            for t in d.order_by.iter_mut() {
                                t.expr = rewrite_expr(&t.expr, p, scope, aggregates, group_by);
                            }
                            d
                        })),
                    })
                }),
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(rewrite_expr(o, p, scope, aggregates, group_by))),
            whens: whens
                .iter()
                .map(|(c, v)| {
                    (
                        rewrite_expr(c, p, scope, aggregates, group_by),
                        rewrite_expr(v, p, scope, aggregates, group_by),
                    )
                })
                .collect(),
            else_: else_
                .as_ref()
                .map(|x| Box::new(rewrite_expr(x, p, scope, aggregates, group_by))),
        },
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|i| rewrite_expr(i, p, scope, aggregates, group_by))
                .collect(),
        ),
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_expr(expr, p, scope, aggregates, group_by)),
            collation: collation.clone(),
        },
        Expr::Raise { action, message } => Expr::Raise {
            action: *action,
            message: message
                .as_ref()
                .map(|m| Box::new(rewrite_expr(m, p, scope, aggregates, group_by))),
        },
        _ => e.clone(),
    }
}

/// Rewrite a whole subquery statement's expression sites. A fresh
/// pooled scope is computed when `scope` is None (a top-level atom);
/// nested atoms pass their outer atom's scope through.
fn rewrite_sel(
    sel: &SelectStatement,
    p: &Planner<'_>,
    scope: Option<&SubqueryScope>,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) -> SelectStatement {
    let mut sel = sel.clone();
    let own_scope;
    let scope = match scope {
        Some(s) => s,
        None => {
            own_scope = SubqueryScope::of(p, &sel);
            &own_scope
        }
    };
    rewrite_body(&mut sel.body, p, scope, aggregates, group_by);
    for t in sel.order_by.iter_mut() {
        t.expr = rewrite_expr(&t.expr, p, Some(scope), aggregates, group_by);
    }
    if let Some(l) = sel.limit.take() {
        sel.limit = Some(rewrite_expr(&l, p, Some(scope), aggregates, group_by));
    }
    if let Some(o) = sel.offset.take() {
        sel.offset = Some(rewrite_expr(&o, p, Some(scope), aggregates, group_by));
    }
    sel
}

fn rewrite_body(
    body: &mut SelectBody,
    p: &Planner<'_>,
    scope: &SubqueryScope,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) {
    let sc = Some(scope);
    match body {
        SelectBody::Simple(s) => {
            for c in s.columns.iter_mut() {
                if let ResultColumn::Expr { expr, .. } = c {
                    *expr = rewrite_expr(expr, p, sc, aggregates, group_by);
                }
            }
            if let Some(w) = s.where_clause.as_mut() {
                *w = rewrite_expr(w, p, sc, aggregates, group_by);
            }
            for g in s.group_by.iter_mut() {
                *g = rewrite_expr(g, p, sc, aggregates, group_by);
            }
            if let Some(h) = s.having.as_mut() {
                *h = rewrite_expr(h, p, sc, aggregates, group_by);
            }
            for def in s.window.iter_mut() {
                for e in def.partition_by.iter_mut() {
                    *e = rewrite_expr(e, p, sc, aggregates, group_by);
                }
                for t in def.order_by.iter_mut() {
                    t.expr = rewrite_expr(&t.expr, p, sc, aggregates, group_by);
                }
            }
            if let Some(from) = s.from.as_mut() {
                rewrite_from(from, p, scope, aggregates, group_by);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            rewrite_body(left, p, scope, aggregates, group_by);
            rewrite_body(right, p, scope, aggregates, group_by);
        }
    }
}

fn rewrite_from(
    te: &mut TableExpression,
    p: &Planner<'_>,
    scope: &SubqueryScope,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) {
    // Join constraints reference the join's sides (in the pooled source
    // set — left for the runtime frames when local) and may leak outer
    // references (`ON x = outer.y`).
    if let TableExpression::Join {
        left,
        right,
        constraint,
        ..
    } = te
    {
        rewrite_from(left, p, scope, aggregates, group_by);
        rewrite_from(right, p, scope, aggregates, group_by);
        if let JoinConstraint::On(c) = constraint {
            *c = rewrite_expr(c, p, Some(scope), aggregates, group_by);
        }
    }
    // FROM-subquery bodies and table-valued function arguments are left
    // verbatim: their references bind within their own derived scope
    // (they are in the pooled source set), and correlated FROM
    // subqueries are not a shape this pass claims.
}
