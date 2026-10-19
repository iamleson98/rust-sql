//! SQLite's aggregate promotion out of subqueries (resolve.c's
//! NameContext climb): an aggregate call inside a subquery whose
//! argument / FILTER / ORDER BY references do NOT resolve against the
//! subquery's own sources — but DO resolve against THIS query's —
//! belongs to THIS query. The subquery expression keeps its shape but
//! the call is computed by THIS query's Aggregate and read back as an
//! output column, exactly like SQLite's AggInfo registration.
//!
//! Observable contract (oracle-mapped against the vendored SQLite):
//!
//! * `SELECT (SELECT sum(a)) FROM t1` — one row (the query BECOMES an
//!   aggregate query; window1 61.4.2's `{0.0}` over empty input).
//! * The promoted call reads back per GROUP with GROUP BY, and a bare
//!   sibling column follows the min/max-achieving row
//!   (`(SELECT min(a)), (SELECT b)`).
//! * Landing zones: the select list, HAVING (of an aggregate query),
//!   ORDER BY (of an aggregate query) and WINDOW-SPEC expressions
//!   (PARTITION BY / ORDER BY inside `OVER (...)`) are legal. WHERE,
//!   ON, GROUP BY, a window function's ARGUMENTS and an aggregate
//!   call's arguments are "misuse of aggregate: {name}()" — the name
//!   as written (window1 38.10's uppercase `AVG()`).
//! * `EXISTS (SELECT sum(a))` never promotes; an IN-source subquery
//!   promotes with its IN's position.
//! * The promotion recurses: a call whose refs bind at an INTERMEDIATE
//!   subquery promotes there (window1 53.0's `sum(d.c)` into the
//!   `FROM a AS d` subquery) — this pass runs per planned SELECT, so
//!   the intermediate's own planning performs that landing.
//!
//! Refs that bind neither inside the atom nor in THIS query's sources
//! (further out) are left to the runtime frame stack — today's
//! per-row behavior — documented as the pass-through boundary.

use crate::planner::expr_eq::expr_eq;
use crate::planner::plan::AggExpr;
use crate::planner::subquery_bare::SubqueryScope;
use crate::planner::Planner;
use crate::schema::Table;
use crate::sql::ast::{
    Expr, InSource, JoinConstraint, OrderTerm, ResultColumn, SelectBody, SelectStatement,
    TableExpression, WindowSpec,
};
use std::collections::HashSet;
use std::sync::Arc;

/// THIS query's own source scope: its FROM tables/aliases (derived
/// tables pooled through their bodies, like every scope model here)
/// plus the visible CTEs — NOT pooled through EXPRESSION subqueries: a
/// sibling atom's tables are invisible here, matching SQLite's
/// per-level NameContext.
pub(crate) struct CurrentScope {
    quals: HashSet<String>,
    tables: Vec<Arc<Table>>,
    /// Derived tables' OUTPUT column names (bare) — `(SELECT a AS b FROM
    /// t0)` in the FROM makes `b` resolvable here (window1 71.0: the
    /// compound subquery's `sum(b)` promotes into this query and the
    /// WHERE landing errors).
    derived_cols: HashSet<String>,
}

impl CurrentScope {
    fn of(p: &Planner<'_>, s: &crate::sql::ast::SimpleSelect) -> Self {
        let mut quals = HashSet::new();
        let mut tables = Vec::new();
        let mut derived_cols = HashSet::new();
        if let Some(from) = &s.from {
            collect_from_sources(from, p, &mut quals, &mut tables, &mut derived_cols);
        }
        Self {
            quals,
            tables,
            derived_cols,
        }
    }

    /// Mirrors `SubqueryScope::is_local` (the same checks the runtime
    /// classifier uses), so a ref classified as promoting here is one
    /// the runtime frame stack would bind here.
    fn resolves(&self, p: &Planner<'_>, table: &Option<String>, name: &str) -> bool {
        match table {
            Some(q) => self.quals.contains(&q.to_ascii_lowercase()),
            None => {
                let known_table = self.tables.iter().any(|t| {
                    t.find_column(name).is_some()
                        || (crate::planner::is_rowid_spelling(name) && !t.without_rowid)
                });
                known_table
                    || self.derived_cols.contains(&name.to_ascii_lowercase())
                    || p.cte_column_in_scope(name)
            }
        }
    }
}

/// FROM-only source walk (a `TableExpression` tree): plain tables
/// contribute name+alias and their schema; derived tables contribute
/// their alias and pool their body's sources.
fn collect_from_sources(
    te: &TableExpression,
    p: &Planner<'_>,
    quals: &mut HashSet<String>,
    tables: &mut Vec<Arc<Table>>,
    derived_cols: &mut HashSet<String>,
) {
    match te {
        TableExpression::Table { name, alias, .. } => {
            quals.insert(
                alias
                    .clone()
                    .unwrap_or_else(|| name.clone())
                    .to_ascii_lowercase(),
            );
            if let Some(t) = p.catalog.get_table(name) {
                tables.push(t);
            }
        }
        TableExpression::Subquery { alias, select, .. } => {
            if let Some(a) = alias {
                quals.insert(a.to_ascii_lowercase());
            }
            // The derived table's OUTPUT names resolve here (bare).
            let mut names = Vec::new();
            collect_body_output_names(&select.body, &mut names);
            for n in names {
                derived_cols.insert(n.to_ascii_lowercase());
            }
            collect_body_sources(&select.body, p, quals, tables, derived_cols);
        }
        TableExpression::Join { left, right, .. } => {
            collect_from_sources(left, p, quals, tables, derived_cols);
            collect_from_sources(right, p, quals, tables, derived_cols);
        }
        TableExpression::Function { .. } => {}
    }
}

fn collect_body_sources(
    body: &SelectBody,
    p: &Planner<'_>,
    quals: &mut HashSet<String>,
    tables: &mut Vec<Arc<Table>>,
    derived_cols: &mut HashSet<String>,
) {
    match body {
        SelectBody::Simple(s) => {
            if let Some(from) = &s.from {
                collect_from_sources(from, p, quals, tables, derived_cols);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_sources(left, p, quals, tables, derived_cols);
            collect_body_sources(right, p, quals, tables, derived_cols);
        }
    }
}

/// A quick pure-AST gate: does `e` contain any subquery atom that could
/// promote (`Expr::Subquery` / IN-subquery — EXISTS never promotes)?
/// The hot path (no subqueries) never builds scopes.
fn has_promotable_atom(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) => true,
        Expr::Exists(_) => false,
        Expr::In { expr, source, .. } => {
            has_promotable_atom(expr) || matches!(source, InSource::Subquery(_))
        }
        _ => expr_children(e).into_iter().any(has_promotable_atom),
    }
}

fn expr_children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Binary { left, right, .. } => vec![left.as_ref(), right.as_ref()],
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => vec![expr.as_ref()],
        Expr::Between {
            expr, low, high, ..
        } => vec![expr.as_ref(), low.as_ref(), high.as_ref()],
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            let mut v = vec![expr.as_ref(), pattern.as_ref()];
            if let Some(x) = escape {
                v.push(x.as_ref());
            }
            v
        }
        Expr::Is { left, right, .. } => vec![left.as_ref(), right.as_ref()],
        Expr::Function {
            args,
            filter,
            order_by,
            over,
            ..
        } => {
            let mut v: Vec<&Expr> = args.iter().collect();
            if let Some(f) = filter {
                v.push(f.as_ref());
            }
            for t in order_by {
                v.push(&t.expr);
            }
            // Window SPEC expressions (PARTITION BY / ORDER BY inside
            // the OVER) are part of the walk: an aggregate inside a
            // subquery's window spec promotes (window1 61.4.2's
            // `count(a) OVER (ORDER BY sum(a))`).
            if let Some(spec) = over {
                if let WindowSpec::Inline(d) = spec.as_ref() {
                    for pe in &d.partition_by {
                        v.push(pe);
                    }
                    for t in &d.order_by {
                        v.push(&t.expr);
                    }
                }
            }
            v
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let mut v: Vec<&Expr> = Vec::new();
            if let Some(o) = operand {
                v.push(o.as_ref());
            }
            for (c, val) in whens {
                v.push(c);
                v.push(val);
            }
            if let Some(x) = else_ {
                v.push(x.as_ref());
            }
            v
        }
        Expr::Row(items) => items.iter().collect(),
        Expr::Raise {
            message: Some(m), ..
        } => vec![m.as_ref()],
        _ => Vec::new(),
    }
}

/// Is `e` (as a whole) an aggregate call — no OVER, the polymorphic
/// min/max arity rule included?
fn is_agg_call(e: &Expr) -> bool {
    let Expr::Function {
        name, args, over, ..
    } = e
    else {
        return false;
    };
    over.is_none() && crate::planner::is_aggregate_call(&name.to_ascii_lowercase(), args.len())
}

/// Every (qual, name) column reference in `e`'s whole subtree,
/// subqueries and compound bodies included (an aggregate's ARG may be
/// a compound subquery reading outer columns — window1 63.3's
/// `sum((SELECT x AS c UNION SELECT 1234 ORDER BY c))`).
fn collect_deep_refs(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => {
            if name != "*" {
                out.push((table.clone(), name.clone()));
            }
        }
        Expr::Subquery(sel) | Expr::Exists(sel) => collect_stmt_refs(sel, out),
        Expr::In { expr, source, .. } => {
            collect_deep_refs(expr, out);
            if let InSource::Subquery(sel) = source {
                collect_stmt_refs(sel, out);
            } else if let InSource::List(items) = source {
                for i in items.iter() {
                    collect_deep_refs(i, out);
                }
            }
        }
        _ => expr_children(e)
            .into_iter()
            .for_each(|c| collect_deep_refs(c, out)),
    }
}

fn collect_stmt_refs(sel: &SelectStatement, out: &mut Vec<(Option<String>, String)>) {
    collect_body_refs_deep(&sel.body, out);
    // Output names of THIS statement (aliases + bare-column arms): a
    // bare reference in its ORDER BY / GROUP BY that matches one
    // resolves LOCALLY (the compound's own output column, window1
    // 63.3's `SELECT x AS c UNION SELECT 1234 ORDER BY c` — the c is
    // arm 1's alias, not an outer column) — not promotion evidence.
    let mut own_names: Vec<String> = Vec::new();
    collect_body_output_names(&sel.body, &mut own_names);
    let is_own = |n: &str| own_names.iter().any(|o| o.eq_ignore_ascii_case(n));
    for t in &sel.order_by {
        if let Expr::Column { table: None, name } = &t.expr {
            if is_own(name) {
                continue;
            }
        }
        collect_deep_refs(&t.expr, out);
    }
    if let Some(l) = &sel.limit {
        collect_deep_refs(l, out);
    }
    if let Some(o) = &sel.offset {
        collect_deep_refs(o, out);
    }
}

/// The output names a nested statement's ORDER BY / GROUP BY may refer
/// to: result aliases plus bare-column select items (all compound arms).
fn collect_body_output_names(body: &SelectBody, out: &mut Vec<String>) {
    match body {
        SelectBody::Simple(s) => {
            for c in &s.columns {
                if let ResultColumn::Expr { expr, alias } = c {
                    if let Some(a) = alias {
                        out.push(a.clone());
                    } else if let Expr::Column { name, .. } = expr {
                        out.push(name.clone());
                    }
                }
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_output_names(left, out);
            collect_body_output_names(right, out);
        }
    }
}

fn collect_body_refs_deep(body: &SelectBody, out: &mut Vec<(Option<String>, String)>) {
    match body {
        SelectBody::Simple(s) => {
            for c in &s.columns {
                if let ResultColumn::Expr { expr, .. } = c {
                    collect_deep_refs(expr, out);
                }
            }
            if let Some(w) = &s.where_clause {
                collect_deep_refs(w, out);
            }
            for g in &s.group_by {
                collect_deep_refs(g, out);
            }
            if let Some(h) = &s.having {
                collect_deep_refs(h, out);
            }
            if let Some(from) = &s.from {
                collect_from_refs(from, out);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            collect_body_refs_deep(left, out);
            collect_body_refs_deep(right, out);
        }
    }
}

fn collect_from_refs(te: &TableExpression, out: &mut Vec<(Option<String>, String)>) {
    match te {
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
            if let JoinConstraint::On(c) = constraint {
                collect_deep_refs(c, out);
            }
            collect_from_refs(left, out);
            collect_from_refs(right, out);
        }
        TableExpression::Subquery { select, .. } => collect_stmt_refs(select, out),
        _ => {}
    }
}

impl Planner<'_> {
    /// The promotion candidates of ONE subquery atom: aggregate calls
    /// anywhere inside the atom's tree with at least one reference that
    /// does not resolve against the atom's POOLED scope (everything the
    /// atom introduces, at any depth) — and all such outside references
    /// resolving against THIS query's sources, so the call lands here.
    fn atom_promoted_calls(&self, atom: &SelectStatement, cur: &CurrentScope, out: &mut Vec<Expr>) {
        let pooled = SubqueryScope::of(self, atom);
        let mut calls: Vec<Expr> = Vec::new();
        scan_stmt_for_promotable(atom, &pooled, self, cur, &mut calls);
        for c in calls {
            if !out.iter().any(|x| expr_eq(x, &c)) {
                out.push(c);
            }
        }
    }

    /// All aggregate calls promoting into THIS select from subquery
    /// atoms at its LEGAL landing sites (select list, HAVING, ORDER BY
    /// terms, and the WINDOW-SPEC expressions of its own window calls).
    /// Deterministic in (s, order_terms) — every
    /// `build_full_agg_set` caller derives the same slots.
    pub(crate) fn subquery_agg_promotions(
        &self,
        s: &crate::sql::ast::SimpleSelect,
        order_terms: &[OrderTerm],
    ) -> Vec<Expr> {
        let mut sites: Vec<&Expr> = Vec::new();
        for c in &s.columns {
            if let ResultColumn::Expr { expr, .. } = c {
                sites.push(expr);
            }
        }
        if let Some(h) = &s.having {
            sites.push(h);
        }
        for t in order_terms {
            sites.push(&t.expr);
        }
        // Fast path: no promotable atoms anywhere at the legal sites.
        if !sites.iter().any(|e| has_promotable_atom(e)) {
            return Vec::new();
        }
        // The top query's own window SPEC expressions (legal landings —
        // `row_number() OVER (PARTITION BY (SELECT sum(a)))`).
        let mut spec_exprs: Vec<&Expr> = Vec::new();
        for e in &sites {
            collect_window_spec_exprs(e, &mut spec_exprs);
        }
        sites.extend(spec_exprs);
        let cur = CurrentScope::of(self, s);
        let mut out: Vec<Expr> = Vec::new();
        for e in &sites {
            self.collect_atom_calls_legal(e, &cur, &mut out);
        }
        out
    }

    /// Walk a legal-site expression: visit subquery atoms EXCEPT those
    /// inside aggregate-call arguments / FILTER or window-call ARGUMENTS
    /// (those landings are misuse — checked separately).
    fn collect_atom_calls_legal(&self, e: &Expr, cur: &CurrentScope, out: &mut Vec<Expr>) {
        match e {
            Expr::Subquery(sel) => self.atom_promoted_calls(sel, cur, out),
            Expr::Exists(_) => {}
            Expr::In { expr, source, .. } => {
                self.collect_atom_calls_legal(expr, cur, out);
                if let InSource::Subquery(sel) = source {
                    self.atom_promoted_calls(sel, cur, out);
                }
            }
            Expr::Function {
                args, filter, over, ..
            } => {
                if over.is_none() && is_agg_call(e) {
                    // Aggregate calls own their args/FILTER: atoms there
                    // land inside the aggregate — misuse, not collected.
                    return;
                }
                if over.is_some() {
                    // A window call's ARGUMENTS are a misuse landing;
                    // its SPEC expressions (PARTITION BY / ORDER BY,
                    // inline or through the WINDOW clause) are legal.
                    if let Some(spec) = over {
                        if let WindowSpec::Inline(d) = spec.as_ref() {
                            for pe in &d.partition_by {
                                self.collect_atom_calls_legal(pe, cur, out);
                            }
                            for t in &d.order_by {
                                self.collect_atom_calls_legal(&t.expr, cur, out);
                            }
                        }
                    }
                    return;
                }
                // A scalar call's arguments are ordinary positions.
                for a in args {
                    self.collect_atom_calls_legal(a, cur, out);
                }
                if let Some(f) = filter {
                    self.collect_atom_calls_legal(f, cur, out);
                }
            }
            _ => expr_children(e)
                .into_iter()
                .for_each(|c| self.collect_atom_calls_legal(c, cur, out)),
        }
    }

    /// SQLite's landing-zone errors for subquery-borne aggregates (the
    /// checks run before any promotion flips the query's shape):
    /// WHERE / ON / GROUP BY, a window call's arguments, an aggregate
    /// call's arguments, and ORDER BY of a query that is not an
    /// aggregate query by other means. Runs at EVERY planned SELECT —
    /// the per-level recursion is what makes window1 53.0's
    /// intermediate-scope landing work (no promotion to the top there,
    /// so no top-level error).
    pub(crate) fn check_subquery_agg_misuse(
        &self,
        s: &crate::sql::ast::SimpleSelect,
        order_terms: &[OrderTerm],
    ) -> crate::error::Result<()> {
        let misuse = |n: &str| crate::error::Error::semantic(format!("misuse of aggregate: {n}()"));
        // Cheap gate: any promotable atom anywhere in the checked sites.
        let any_atom = |e: &Expr| has_promotable_atom(e);
        let where_has = s.where_clause.as_ref().is_some_and(any_atom);
        let group_has = s.group_by.iter().any(any_atom);
        let mut on_exprs: Vec<&Expr> = Vec::new();
        if let Some(from) = &s.from {
            collect_on_exprs(from, &mut on_exprs);
        }
        let on_has = on_exprs.iter().any(|e| any_atom(e));
        let cols_have = s
            .columns
            .iter()
            .any(|c| matches!(c, ResultColumn::Expr { expr, .. } if any_atom(expr)));
        let having_has = s.having.as_ref().is_some_and(any_atom);
        let order_has = order_terms.iter().any(|t| any_atom(&t.expr));
        if !(where_has || group_has || on_has || cols_have || having_has || order_has) {
            return Ok(());
        }
        let cur = CurrentScope::of(self, s);
        let mut probe: Vec<Expr> = Vec::new();
        // WHERE / ON: any landing is misuse.
        if let Some(w) = &s.where_clause {
            self.collect_atom_calls_full(w, &cur, &mut probe);
        }
        for on in on_exprs {
            self.collect_atom_calls_full(on, &cur, &mut probe);
        }
        if let Some(call) = probe.first() {
            let Expr::Function { name, .. } = call else {
                unreachable!("promoted calls are Function nodes");
            };
            return Err(misuse(name));
        }
        // GROUP BY: SQLite's GROUP BY-specific message.
        probe.clear();
        for g in &s.group_by {
            self.collect_atom_calls_full(g, &cur, &mut probe);
        }
        if !probe.is_empty() {
            return Err(crate::error::Error::semantic(
                "aggregate functions are not allowed in the GROUP BY clause",
            ));
        }
        // Aggregate / window call ARGUMENTS at the legal sites: a
        // promoting call inside them is misuse (window1 59.1's
        // `ntile((SELECT sum(x)))`, the `max((SELECT sum(a)))` shape).
        probe.clear();
        let mut scan_args = |e: &Expr| self.scan_call_args_for_atoms(e, &cur, &mut probe);
        for c in &s.columns {
            if let ResultColumn::Expr { expr, .. } = c {
                scan_args(expr);
            }
        }
        if let Some(h) = &s.having {
            scan_args(h);
        }
        for t in order_terms {
            scan_args(&t.expr);
        }
        if let Some(call) = probe.first() {
            let Expr::Function { name, .. } = call else {
                unreachable!("promoted calls are Function nodes");
            };
            return Err(misuse(name));
        }
        // ORDER BY of a query that is not an aggregate query by other
        // means (GROUP BY / HAVING / its own direct aggregates): the
        // promotion does not rescue the landing (oracle: `SELECT a FROM
        // t ORDER BY (SELECT sum(a))` errors; `SELECT sum(a) FROM t
        // ORDER BY (SELECT sum(a))` does not).
        let otherwise_agg = !s.group_by.is_empty()
            || s.having.is_some()
            || self.expr_list_has_aggregates(&s.columns);
        if !otherwise_agg {
            probe.clear();
            for t in order_terms {
                self.collect_atom_calls_legal(&t.expr, &cur, &mut probe);
            }
            if let Some(call) = probe.first() {
                let Expr::Function { name, .. } = call else {
                    unreachable!("promoted calls are Function nodes");
                };
                return Err(misuse(name));
            }
        }
        Ok(())
    }

    /// Full walk (aggregate / window arguments included) — for the
    /// always-illegal sites (WHERE / ON / GROUP BY).
    fn collect_atom_calls_full(&self, e: &Expr, cur: &CurrentScope, out: &mut Vec<Expr>) {
        match e {
            Expr::Subquery(sel) => self.atom_promoted_calls(sel, cur, out),
            Expr::Exists(_) => {}
            Expr::In { expr, source, .. } => {
                self.collect_atom_calls_full(expr, cur, out);
                if let InSource::Subquery(sel) = source {
                    self.atom_promoted_calls(sel, cur, out);
                }
            }
            _ => expr_children(e)
                .into_iter()
                .for_each(|c| self.collect_atom_calls_full(c, cur, out)),
        }
    }

    /// The arguments / FILTER of aggregate and window calls at a legal
    /// site: subquery atoms inside them carry misuse landings.
    fn scan_call_args_for_atoms(&self, e: &Expr, cur: &CurrentScope, out: &mut Vec<Expr>) {
        if let Expr::Function {
            args, filter, over, ..
        } = e
        {
            if over.is_some() || is_agg_call(e) {
                for a in args {
                    self.collect_atom_calls_full(a, cur, out);
                }
                if let Some(f) = filter {
                    self.collect_atom_calls_full(f, cur, out);
                }
                return;
            }
        }
        expr_children(e)
            .into_iter()
            .for_each(|c| self.scan_call_args_for_atoms(c, cur, out));
    }
}

/// Walk a statement's WHOLE tree (every clause, nested atoms, compound
/// arms, derived-table bodies) for aggregate calls whose refs bind
/// outside the atom's pooled scope and inside `cur`.
fn scan_stmt_for_promotable(
    sel: &SelectStatement,
    pooled: &SubqueryScope,
    p: &Planner<'_>,
    cur: &CurrentScope,
    out: &mut Vec<Expr>,
) {
    scan_body_for_promotable(&sel.body, pooled, p, cur, out);
    for t in &sel.order_by {
        scan_expr_for_promotable(&t.expr, pooled, p, cur, out);
    }
    if let Some(l) = &sel.limit {
        scan_expr_for_promotable(l, pooled, p, cur, out);
    }
    if let Some(o) = &sel.offset {
        scan_expr_for_promotable(o, pooled, p, cur, out);
    }
}

fn scan_body_for_promotable(
    body: &SelectBody,
    pooled: &SubqueryScope,
    p: &Planner<'_>,
    cur: &CurrentScope,
    out: &mut Vec<Expr>,
) {
    match body {
        SelectBody::Simple(s) => {
            for c in &s.columns {
                if let ResultColumn::Expr { expr, .. } = c {
                    scan_expr_for_promotable(expr, pooled, p, cur, out);
                }
            }
            if let Some(w) = &s.where_clause {
                scan_expr_for_promotable(w, pooled, p, cur, out);
            }
            for g in &s.group_by {
                scan_expr_for_promotable(g, pooled, p, cur, out);
            }
            if let Some(h) = &s.having {
                scan_expr_for_promotable(h, pooled, p, cur, out);
            }
            if let Some(from) = &s.from {
                scan_from_for_promotable(from, pooled, p, cur, out);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            scan_body_for_promotable(left, pooled, p, cur, out);
            scan_body_for_promotable(right, pooled, p, cur, out);
        }
    }
}

fn scan_from_for_promotable(
    te: &TableExpression,
    pooled: &SubqueryScope,
    p: &Planner<'_>,
    cur: &CurrentScope,
    out: &mut Vec<Expr>,
) {
    match te {
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
            if let JoinConstraint::On(c) = constraint {
                scan_expr_for_promotable(c, pooled, p, cur, out);
            }
            scan_from_for_promotable(left, pooled, p, cur, out);
            scan_from_for_promotable(right, pooled, p, cur, out);
        }
        // Derived-table bodies: an aggregate inside a FROM-subquery
        // whose refs bind outside the whole atom still promotes
        // (window1 57.3's `(SELECT x FROM (SELECT sum(y) AS x FROM t1))`
        // — t1 has no y, so sum(y) belongs to the t3 query).
        TableExpression::Subquery { select, .. } => {
            scan_stmt_for_promotable(select, pooled, p, cur, out);
        }
        _ => {}
    }
}

fn scan_expr_for_promotable(
    e: &Expr,
    pooled: &SubqueryScope,
    p: &Planner<'_>,
    cur: &CurrentScope,
    out: &mut Vec<Expr>,
) {
    if is_agg_call(e) {
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        if let Expr::Function {
            args,
            filter,
            order_by,
            ..
        } = e
        {
            for a in args {
                collect_deep_refs(a, &mut refs);
            }
            if let Some(f) = filter {
                collect_deep_refs(f, &mut refs);
            }
            for t in order_by {
                collect_deep_refs(&t.expr, &mut refs);
            }
        }
        let mut all_bind_here = true;
        let mut any_local = false;
        for (t, n) in &refs {
            if pooled.is_local(p, t, n) {
                // A MIXED aggregate (some refs local to the atom, some
                // outer) belongs to the ATOM's own query — a correlated
                // aggregate, evaluated by the subquery's planning
                // (oracle: `SELECT (SELECT sum(b+a) FROM t2) FROM t1`
                // keeps {34, 36}, `avg(c1) FILTER(WHERE t1.a=1) FROM
                // t2` stays per-row).
                any_local = true;
                break;
            }
            if !cur.resolves(p, t, n) {
                all_bind_here = false;
                break;
            }
        }
        if !any_local && all_bind_here && !refs.is_empty() {
            out.push(e.clone());
        }
        // The call's args may still hold deeper atoms whose own
        // aggregates promote (rare; keep the walk complete).
        expr_children(e)
            .into_iter()
            .for_each(|c| scan_expr_for_promotable(c, pooled, p, cur, out));
        return;
    }
    match e {
        Expr::Subquery(sel) | Expr::Exists(sel) => {
            // Nested atoms share the outer atom's POOLED scope (the
            // win-4a model): their sources are already in the pool.
            scan_stmt_for_promotable(sel, pooled, p, cur, out);
        }
        Expr::In { expr, source, .. } => {
            scan_expr_for_promotable(expr, pooled, p, cur, out);
            if let InSource::Subquery(sel) = source {
                scan_stmt_for_promotable(sel, pooled, p, cur, out);
            } else if let InSource::List(items) = source {
                for i in items.iter() {
                    scan_expr_for_promotable(i, pooled, p, cur, out);
                }
            }
        }
        _ => expr_children(e)
            .into_iter()
            .for_each(|c| scan_expr_for_promotable(c, pooled, p, cur, out)),
    }
}

/// The INLINE window-spec expressions (PARTITION BY / ORDER BY) of
/// every window call in `e` (top-level atoms' own windows — their
/// specs may hold promotable subqueries).
fn collect_window_spec_exprs<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Function {
        over: Some(spec), ..
    } = e
    {
        if let WindowSpec::Inline(d) = spec.as_ref() {
            for pe in &d.partition_by {
                out.push(pe);
            }
            for t in &d.order_by {
                out.push(&t.expr);
            }
        }
    }
    expr_children(e)
        .into_iter()
        .for_each(|c| collect_window_spec_exprs(c, out));
}

fn collect_on_exprs<'a>(te: &'a TableExpression, out: &mut Vec<&'a Expr>) {
    if let TableExpression::Join {
        left,
        right,
        constraint,
        ..
    } = te
    {
        if let JoinConstraint::On(c) = constraint {
            out.push(c);
        }
        collect_on_exprs(left, out);
        collect_on_exprs(right, out);
    }
}

/// Decompose a promoted call into its AggExpr through the SAME
/// collector the direct path uses (`collect_aggregates_rec`), so the
/// slot identity and the rewrite's matcher agree exactly.
pub(crate) fn promoted_call_aggexpr(call: &Expr) -> Option<AggExpr> {
    let mut out: Vec<AggExpr> = Vec::new();
    crate::planner::collect_aggregates_rec_pub(call, &None, &mut out);
    out.into_iter().next()
}

/// Do two aggregate sets describe the same call (the dedup key of the
/// direct collector and the ORDER-only merge)?
pub(crate) fn agg_same_call(a: &AggExpr, b: &AggExpr) -> bool {
    a.func == b.func
        && a.distinct == b.distinct
        && a.sep == b.sep
        && match (&a.arg, &b.arg) {
            (Some(x), Some(y)) => expr_eq(x, y),
            (None, None) => true,
            _ => false,
        }
        && match (&a.filter, &b.filter) {
            (Some(x), Some(y)) => expr_eq(x, y),
            (None, None) => true,
            _ => false,
        }
        && a.order_by.len() == b.order_by.len()
        && a.order_by
            .iter()
            .zip(b.order_by.iter())
            .all(|(x, y)| expr_eq(&x.expr, &y.expr) && x.order == y.order && x.nulls == y.nulls)
}

// ---------------------------------------------------------------------------
// The rewrite: promoted calls inside subquery atoms become `__agg_N`
// output-column reads, so the runtime frame stack binds them against
// the Aggregate's output row (the same mechanism as the bare-column
// rewrite of planner::subquery_bare).
// ---------------------------------------------------------------------------

impl Planner<'_> {
    /// Rewrite promoted aggregate calls INSIDE `e`'s subquery atoms to
    /// their `__agg_N` output columns. Runs BEFORE the bare-column
    /// rewrite (a promoted call's argument belongs to the aggregate —
    /// per-INPUT-row — and must not become a bare slot reference).
    pub(crate) fn rewrite_subquery_agg_calls(
        &self,
        e: &Expr,
        aggregates: &[AggExpr],
        group_by: &[Expr],
    ) -> Expr {
        rewrite_agg_expr(e, self, None, aggregates, group_by)
    }
}

fn rewrite_agg_expr(
    e: &Expr,
    p: &Planner<'_>,
    scope: Option<&SubqueryScope>,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) -> Expr {
    // Inside an atom: an aggregate call with outside refs that matches
    // a slot becomes the slot read. Group-term cutoffs do NOT apply to
    // aggregate calls (a promoted `sum(a)` under `GROUP BY a` is the
    // per-group SUM, not the group key).
    if let Some(pooled) = scope.filter(|_| is_agg_call(e)) {
        let mut refs: Vec<(Option<String>, String)> = Vec::new();
        if let Expr::Function {
            args,
            filter,
            order_by,
            ..
        } = e
        {
            for a in args {
                collect_deep_refs(a, &mut refs);
            }
            if let Some(f) = filter {
                collect_deep_refs(f, &mut refs);
            }
            for t in order_by {
                collect_deep_refs(&t.expr, &mut refs);
            }
        }
        // Promote only the calls the collector would promote: ALL refs
        // non-local (a mixed aggregate stays inside the atom).
        let all_outer = !refs.is_empty() && refs.iter().all(|(t, n)| !pooled.is_local(p, t, n));
        if all_outer {
            if let Some(i) = crate::planner::aggregate_slot_for_call(e, aggregates) {
                let slot = Expr::Column {
                    table: None,
                    name: format!("__agg_{}", i),
                };
                return crate::planner::collating_slot_read(slot, e);
            }
        }
        // No slot: the call stays (pass-through binding, or a local
        // aggregate of a deeper subquery evaluated by its own planning).
        return e.clone();
    }
    match e {
        Expr::Subquery(sel) => Expr::Subquery(Box::new(rewrite_agg_sel(
            sel, p, scope, aggregates, group_by,
        ))),
        Expr::Exists(sel) => Expr::Exists(Box::new(rewrite_agg_sel(
            sel, p, scope, aggregates, group_by,
        ))),
        Expr::In {
            expr,
            source,
            negated,
        } => {
            let source = match source {
                InSource::Subquery(sel) => InSource::Subquery(Box::new(rewrite_agg_sel(
                    sel, p, scope, aggregates, group_by,
                ))),
                InSource::List(items) => InSource::List(std::sync::Arc::new(
                    items
                        .iter()
                        .map(|i| rewrite_agg_expr(i, p, scope, aggregates, group_by))
                        .collect(),
                )),
                other => other.clone(),
            };
            Expr::In {
                expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
                source,
                negated: *negated,
            }
        }
        Expr::Function {
            name,
            distinct,
            args,
            filter,
            over,
            order_by,
        } => {
            // The args of aggregate/window calls in TOP mode stay
            // verbatim only when the misuse check passed them; atoms
            // there were rejected at planning, so walking is safe —
            // but the promoted call inside them never matched a slot
            // (they were never collected), so the walk is also inert.
            let mut new_over = None;
            if let Some(spec) = over {
                new_over = Some(Box::new(match spec.as_ref() {
                    WindowSpec::Named(n) => WindowSpec::Named(n.clone()),
                    WindowSpec::Inline(d) => WindowSpec::Inline(Box::new({
                        let mut nd = d.as_ref().clone();
                        for pe in nd.partition_by.iter_mut() {
                            *pe = rewrite_agg_expr(pe, p, scope, aggregates, group_by);
                        }
                        for t in nd.order_by.iter_mut() {
                            t.expr = rewrite_agg_expr(&t.expr, p, scope, aggregates, group_by);
                        }
                        nd
                    })),
                }));
            }
            Expr::Function {
                name: name.clone(),
                distinct: *distinct,
                args: args
                    .iter()
                    .map(|a| rewrite_agg_expr(a, p, scope, aggregates, group_by))
                    .collect(),
                filter: filter
                    .as_ref()
                    .map(|f| Box::new(rewrite_agg_expr(f, p, scope, aggregates, group_by))),
                order_by: order_by
                    .iter()
                    .map(|t| {
                        let mut t = t.clone();
                        t.expr = rewrite_agg_expr(&t.expr, p, scope, aggregates, group_by);
                        t
                    })
                    .collect(),
                over: new_over,
            }
        }
        _ => {
            // Structural recursion for the remaining shapes.
            let rebuilt = e.clone();
            match rebuilt {
                Expr::Binary { .. }
                | Expr::Unary { .. }
                | Expr::IsNull { .. }
                | Expr::Cast { .. }
                | Expr::Collate { .. }
                | Expr::Between { .. }
                | Expr::Like { .. }
                | Expr::Is { .. }
                | Expr::Case { .. }
                | Expr::Row(_) => rebuild_children(e, p, scope, aggregates, group_by),
                _ => e.clone(),
            }
        }
    }
}

/// Rebuild a non-atom, non-function node with rewritten children.
fn rebuild_children(
    e: &Expr,
    p: &Planner<'_>,
    scope: Option<&SubqueryScope>,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) -> Expr {
    match e {
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(rewrite_agg_expr(left, p, scope, aggregates, group_by)),
            right: Box::new(rewrite_agg_expr(right, p, scope, aggregates, group_by)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
            negated: *negated,
        },
        Expr::Cast { expr, type_name } => Expr::Cast {
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
            type_name: type_name.clone(),
        },
        Expr::Collate { expr, collation } => Expr::Collate {
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
            collation: collation.clone(),
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
            low: Box::new(rewrite_agg_expr(low, p, scope, aggregates, group_by)),
            high: Box::new(rewrite_agg_expr(high, p, scope, aggregates, group_by)),
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
            expr: Box::new(rewrite_agg_expr(expr, p, scope, aggregates, group_by)),
            pattern: Box::new(rewrite_agg_expr(pattern, p, scope, aggregates, group_by)),
            escape: escape
                .as_ref()
                .map(|x| Box::new(rewrite_agg_expr(x, p, scope, aggregates, group_by))),
            negated: *negated,
        },
        Expr::Is {
            left,
            right,
            negated,
        } => Expr::Is {
            left: Box::new(rewrite_agg_expr(left, p, scope, aggregates, group_by)),
            right: Box::new(rewrite_agg_expr(right, p, scope, aggregates, group_by)),
            negated: *negated,
        },
        Expr::Case {
            operand,
            whens,
            else_,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(rewrite_agg_expr(o, p, scope, aggregates, group_by))),
            whens: whens
                .iter()
                .map(|(c, v)| {
                    (
                        rewrite_agg_expr(c, p, scope, aggregates, group_by),
                        rewrite_agg_expr(v, p, scope, aggregates, group_by),
                    )
                })
                .collect(),
            else_: else_
                .as_ref()
                .map(|x| Box::new(rewrite_agg_expr(x, p, scope, aggregates, group_by))),
        },
        Expr::Row(items) => Expr::Row(
            items
                .iter()
                .map(|i| rewrite_agg_expr(i, p, scope, aggregates, group_by))
                .collect(),
        ),
        _ => e.clone(),
    }
}

fn rewrite_agg_sel(
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
    rewrite_agg_body(&mut sel.body, p, scope, aggregates, group_by);
    for t in sel.order_by.iter_mut() {
        t.expr = rewrite_agg_expr(&t.expr, p, Some(scope), aggregates, group_by);
    }
    if let Some(l) = sel.limit.take() {
        sel.limit = Some(rewrite_agg_expr(&l, p, Some(scope), aggregates, group_by));
    }
    if let Some(o) = sel.offset.take() {
        sel.offset = Some(rewrite_agg_expr(&o, p, Some(scope), aggregates, group_by));
    }
    sel
}

fn rewrite_agg_body(
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
                    *expr = rewrite_agg_expr(expr, p, sc, aggregates, group_by);
                }
            }
            if let Some(w) = s.where_clause.as_mut() {
                *w = rewrite_agg_expr(w, p, sc, aggregates, group_by);
            }
            for g in s.group_by.iter_mut() {
                *g = rewrite_agg_expr(g, p, sc, aggregates, group_by);
            }
            if let Some(h) = s.having.as_mut() {
                *h = rewrite_agg_expr(h, p, sc, aggregates, group_by);
            }
            if let Some(from) = s.from.as_mut() {
                rewrite_agg_from(from, p, scope, aggregates, group_by);
            }
        }
        SelectBody::Binary { left, right, .. } => {
            rewrite_agg_body(left, p, scope, aggregates, group_by);
            rewrite_agg_body(right, p, scope, aggregates, group_by);
        }
    }
}

fn rewrite_agg_from(
    te: &mut TableExpression,
    p: &Planner<'_>,
    scope: &SubqueryScope,
    aggregates: &[AggExpr],
    group_by: &[Expr],
) {
    match te {
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
            rewrite_agg_from(left, p, scope, aggregates, group_by);
            rewrite_agg_from(right, p, scope, aggregates, group_by);
            if let JoinConstraint::On(c) = constraint {
                *c = rewrite_agg_expr(c, p, Some(scope), aggregates, group_by);
            }
        }
        // Derived-table bodies carry promoted calls too (the scan's
        // FROM-subquery recursion — see scan_from_for_promotable).
        TableExpression::Subquery { select, .. } => {
            rewrite_agg_sel(select, p, Some(scope), aggregates, group_by);
        }
        _ => {}
    }
}
