//! Result-column aliases inside WHERE / ON / GROUP BY / HAVING / ORDER BY
//! expressions — the `NC_UEList` step of SQLite's `lookupName`.
//!
//! An unqualified name that matches no column of the FROM clause but
//! matches a result alias means the aliased expression:
//! `SELECT f1-22 AS x FROM t WHERE x > 0` filters on `f1-22`, and
//! `ORDER BY -x` sorts on `-(f1-22)`. A real column always wins over an
//! alias here (unlike a bare ORDER BY term, which `resolve_order_by_terms`
//! matches against the aliases first). An alias of an aggregate is a
//! "misuse of aliased aggregate" where aggregates are not allowed — the
//! WHERE / ON clauses and the arguments of another aggregate.
//!
//! The rewrite applies only when every FROM item's columns are statically
//! known: otherwise an alias could shadow a real column it cannot see.
//! Subqueries are their own scope and are left alone.

use super::Planner;
use crate::error::{Error, Result};
use crate::sql::ast::*;

type Aliases<'a> = [(&'a str, &'a Expr)];

impl Planner<'_> {
    /// `s` with its alias references substituted, or `None` when nothing
    /// changes.
    pub(super) fn resolve_result_aliases(&self, s: &SimpleSelect) -> Result<Option<SimpleSelect>> {
        let Some(live) = self.live_aliases(s) else {
            return Ok(None);
        };
        let mut changed = false;
        let mut out = s.clone();
        if let Some(w) = &s.where_clause {
            out.where_clause = Some(subst(w, &live, false, false, &mut changed)?);
        }
        if let Some(f) = &s.from {
            out.from = Some(subst_on(f, &live, &mut changed)?);
        }
        for g in out.group_by.iter_mut() {
            // A bare GROUP BY alias is resolved (with its own checks) by
            // the GROUP BY path; nested ones are substituted here.
            if !matches!(g, Expr::Column { table: None, .. }) {
                *g = subst(g, &live, true, false, &mut changed)?;
            }
        }
        if let Some(h) = &s.having {
            out.having = Some(subst(h, &live, true, false, &mut changed)?);
        }
        Ok(changed.then_some(out))
    }

    /// The ORDER BY terms of `s` with alias references INSIDE expressions
    /// substituted (a bare alias term is `resolve_order_by_terms`' job).
    pub(super) fn resolve_order_aliases(
        &self,
        s: &SimpleSelect,
        terms: &[OrderTerm],
    ) -> Result<Option<Vec<OrderTerm>>> {
        let Some(live) = self.live_aliases(s) else {
            return Ok(None);
        };
        let mut changed = false;
        let mut out = terms.to_vec();
        for t in out.iter_mut() {
            let mut base = &t.expr;
            while let Expr::Collate { expr, .. } = base {
                base = expr;
            }
            if matches!(base, Expr::Column { table: None, .. } | Expr::Literal(_)) {
                continue;
            }
            t.expr = subst(&t.expr, &live, true, true, &mut changed)?;
        }
        Ok(changed.then_some(out))
    }

    /// The aliases no FROM column shadows; `None` when there are none or
    /// the FROM clause's columns are not all statically known.
    fn live_aliases<'s>(&self, s: &'s SimpleSelect) -> Option<Vec<(&'s str, &'s Expr)>> {
        let aliases: Vec<(&str, &Expr)> = s
            .columns
            .iter()
            .filter_map(|c| match c {
                ResultColumn::Expr {
                    expr,
                    alias: Some(a),
                } => Some((a.as_str(), expr)),
                _ => None,
            })
            .collect();
        if aliases.is_empty() {
            return None;
        }
        let cols = match &s.from {
            Some(f) => {
                let mut cols = Vec::new();
                self.source_column_names(f, &mut cols)?;
                cols
            }
            None => Vec::new(),
        };
        let live: Vec<(&str, &Expr)> = aliases
            .into_iter()
            .filter(|(a, _)| {
                !cols.iter().any(|c| c.eq_ignore_ascii_case(a))
                    && !(s.from.is_some()
                        && ["rowid", "oid", "_rowid_"]
                            .iter()
                            .any(|r| r.eq_ignore_ascii_case(a)))
            })
            .collect();
        (!live.is_empty()).then_some(live)
    }

    fn source_column_names(&self, te: &TableExpression, out: &mut Vec<String>) -> Option<()> {
        match te {
            TableExpression::Join { left, right, .. } => {
                self.source_column_names(left, out)?;
                self.source_column_names(right, out)
            }
            TableExpression::Table { name, schema, .. } => {
                if schema.is_some() {
                    return None;
                }
                if let Some((_, cols, _)) = self.effective_cte(&name.to_ascii_lowercase()) {
                    out.extend(cols.iter().map(|c| {
                        c.rsplit_once('.')
                            .map_or(c.as_str(), |(_, n)| n)
                            .to_string()
                    }));
                    return Some(());
                }
                if let Some(t) = self.catalog.get_table(name) {
                    if t.vtab.is_some() {
                        return None;
                    }
                    out.extend(t.columns.iter().map(|c| c.name.clone()));
                    return Some(());
                }
                let v = self.catalog.get_view(name)?;
                match &v.columns {
                    Some(c) => out.extend(c.iter().cloned()),
                    None => out.extend(super::subquery_atom_output_names(self.catalog, &v.select)?),
                }
                Some(())
            }
            TableExpression::Subquery {
                select,
                column_aliases,
                ..
            } => {
                match column_aliases {
                    Some(c) => out.extend(c.iter().cloned()),
                    None => out.extend(super::subquery_atom_output_names(self.catalog, select)?),
                }
                Some(())
            }
            TableExpression::Function { .. } => None,
        }
    }
}

fn subst_on(te: &TableExpression, live: &Aliases, changed: &mut bool) -> Result<TableExpression> {
    Ok(match te {
        TableExpression::Join {
            left,
            right,
            join_type,
            constraint,
        } => TableExpression::Join {
            left: Box::new(subst_on(left, live, changed)?),
            right: Box::new(subst_on(right, live, changed)?),
            join_type: *join_type,
            constraint: match constraint {
                JoinConstraint::On(e) => JoinConstraint::On(subst(e, live, false, false, changed)?),
                other => other.clone(),
            },
        },
        other => other.clone(),
    })
}

/// Substitute alias references in `e`. `allow_agg` = an aggregate alias
/// is legal here (HAVING / ORDER BY, outside another aggregate's
/// arguments).
fn subst(
    e: &Expr,
    live: &Aliases,
    allow_agg: bool,
    allow_win: bool,
    changed: &mut bool,
) -> Result<Expr> {
    if let Expr::Column { table: None, name } = e {
        let Some((_, target)) = live.iter().find(|(a, _)| a.eq_ignore_ascii_case(name)) else {
            return Ok(e.clone());
        };
        if !allow_agg && super::expr_has_aggregate(target) {
            return Err(Error::semantic(format!(
                "misuse of aliased aggregate {name}"
            )));
        }
        if !allow_win && super::expr_has_window(target) {
            return Err(Error::semantic(format!(
                "misuse of aliased window function {name}"
            )));
        }
        *changed = true;
        return Ok((*target).clone());
    }
    let inner_agg = match e {
        Expr::Function {
            name, args, over, ..
        } => {
            !(over.is_none() && super::is_aggregate_call(&name.to_ascii_lowercase(), args.len()))
                && allow_agg
        }
        _ => allow_agg,
    };
    crate::executor::map_expr_children(e, &mut |c| subst(c, live, inner_agg, allow_win, changed))
}

/// One result column of a compound arm, `*` expanded: its AS name, the
/// qualifier of a star-expanded column, and its expression.
struct ArmColumn {
    alias: Option<String>,
    qualifier: Option<String>,
    expr: Expr,
}

impl Planner<'_> {
    /// SQLite's `resolveCompoundOrderBy`: each compound ORDER BY term
    /// becomes the ordinal of the result column it names. The arms are
    /// tried leftmost first; in each, a bare name matches an AS alias,
    /// else the term must equal one of the arm's result expressions —
    /// `SELECT a FROM t UNION SELECT b FROM u ORDER BY b` sorts on the
    /// first column. No match anywhere is an error. `None` = leave the
    /// terms alone (an arm whose columns are not statically known).
    pub(super) fn resolve_compound_order(
        &self,
        body: &SelectBody,
        terms: &[OrderTerm],
    ) -> Result<Option<Vec<OrderTerm>>> {
        let mut arms = Vec::new();
        flatten_arms(body, &mut arms);
        let mut expanded = Vec::with_capacity(arms.len());
        for a in &arms {
            let Some(cols) = self.arm_columns(a) else {
                return Ok(None);
            };
            expanded.push((cols, self.arm_qualifiers(a)));
        }
        let width = expanded[0].0.len();
        let mut out = Vec::with_capacity(terms.len());
        for (i, t) in terms.iter().enumerate() {
            // The match ignores a COLLATE wrapper, which still applies to
            // the sort (sqlite3ExprSkipCollate).
            let mut collations = Vec::new();
            let mut base = &t.expr;
            while let Expr::Collate { expr, collation } = base {
                collations.push(collation.clone());
                base = expr;
            }
            let k = if let Some(k) = super::ordinal_term(base) {
                if k < 1 || k as usize > width {
                    return Err(Error::semantic(format!(
                        "{} ORDER BY term out of range - should be between 1 and {width}",
                        super::sqlite_ordinal(i as i64 + 1)
                    )));
                }
                k as usize
            } else {
                let hit = expanded.iter().find_map(|(cols, quals)| {
                    if let Expr::Column { table: None, name } = base {
                        if let Some(p) = cols.iter().position(|c| {
                            c.alias
                                .as_deref()
                                .is_some_and(|a| a.eq_ignore_ascii_case(name))
                        }) {
                            return Some(p);
                        }
                    }
                    cols.iter().position(|c| term_matches(base, c, quals))
                });
                match hit {
                    Some(p) => p + 1,
                    None => {
                        return Err(Error::semantic(format!(
                            "{} ORDER BY term does not match any column in the result set",
                            super::sqlite_ordinal(i as i64 + 1)
                        )))
                    }
                }
            };
            // The sort reads a bare integer as an ordinal, but a COLLATE
            // over one as a constant: a collated term names the leftmost
            // arm's output column instead (left as written when that
            // column has no plain name).
            let mut expr = if collations.is_empty() {
                Expr::Literal(crate::types::Value::Integer(k as i64))
            } else {
                match &expanded[0].0[k - 1] {
                    ArmColumn { alias: Some(a), .. } => Expr::Column {
                        table: None,
                        name: a.clone(),
                    },
                    ArmColumn {
                        alias: None,
                        expr: Expr::Column { name, .. },
                        ..
                    } => Expr::Column {
                        table: None,
                        name: name.clone(),
                    },
                    _ => {
                        out.push(t.clone());
                        continue;
                    }
                }
            };
            for c in collations.into_iter().rev() {
                expr = Expr::Collate {
                    expr: Box::new(expr),
                    collation: c,
                };
            }
            out.push(OrderTerm {
                expr,
                order: t.order,
                nulls: t.nulls,
            });
        }
        Ok(Some(out))
    }

    fn arm_columns(&self, s: &SimpleSelect) -> Option<Vec<ArmColumn>> {
        let mut atoms = Vec::new();
        if let Some(f) = &s.from {
            self.source_atoms(f, &mut atoms)?;
        }
        let mut out = Vec::new();
        for c in &s.columns {
            match c {
                ResultColumn::Expr { expr, alias } => out.push(ArmColumn {
                    alias: alias.clone(),
                    qualifier: None,
                    expr: expr.clone(),
                }),
                ResultColumn::Star => {
                    if atoms.is_empty() {
                        return None;
                    }
                    for (q, cols) in &atoms {
                        out.extend(cols.iter().map(|n| star_column(q, n)));
                    }
                }
                ResultColumn::TableStar(t) => {
                    let (q, cols) = atoms.iter().find(|(q, _)| q.eq_ignore_ascii_case(t))?;
                    out.extend(cols.iter().map(|n| star_column(q, n)));
                }
            }
        }
        Some(out)
    }

    fn arm_qualifiers(&self, s: &SimpleSelect) -> Vec<String> {
        let mut atoms = Vec::new();
        if let Some(f) = &s.from {
            let _ = self.source_atoms(f, &mut atoms);
        }
        atoms.into_iter().map(|(q, _)| q).collect()
    }

    fn source_atoms(
        &self,
        te: &TableExpression,
        out: &mut Vec<(String, Vec<String>)>,
    ) -> Option<()> {
        if let TableExpression::Join { left, right, .. } = te {
            self.source_atoms(left, out)?;
            return self.source_atoms(right, out);
        }
        out.push(self.atom_columns(te)?);
        Some(())
    }
}

fn star_column(q: &str, n: &str) -> ArmColumn {
    ArmColumn {
        alias: None,
        qualifier: Some(q.to_string()),
        expr: Expr::Column {
            table: Some(q.to_string()),
            name: n.to_string(),
        },
    }
}

fn flatten_arms<'a>(body: &'a SelectBody, out: &mut Vec<&'a SimpleSelect>) {
    match body {
        SelectBody::Simple(s) => out.push(s),
        SelectBody::Binary { left, right, .. } => {
            flatten_arms(left, out);
            flatten_arms(right, out);
        }
    }
}

/// `sqlite3ExprCompare` of an ORDER BY term resolved in the arm against
/// one of its result expressions — column references compare by name,
/// qualifiers matching the arm's FROM items.
fn term_matches(term: &Expr, col: &ArmColumn, quals: &[String]) -> bool {
    let norm = |e: &Expr, default_q: Option<&str>| -> Option<String> {
        let mut bad = false;
        let n = normalize_columns(e, default_q, quals, &mut bad);
        (!bad).then(|| format!("{n:?}"))
    };
    let lhs = norm(term, None);
    let rhs = norm(&col.expr, col.qualifier.as_deref());
    lhs.is_some() && lhs == rhs
}

/// Column references with their qualifier dropped (an unknown qualifier
/// marks the term unresolvable) and their name lower-cased.
fn normalize_columns(e: &Expr, default_q: Option<&str>, quals: &[String], bad: &mut bool) -> Expr {
    if let Expr::Column { table, name } = e {
        if let Some(t) = table {
            let known = quals.iter().any(|q| q.eq_ignore_ascii_case(t))
                || default_q.is_some_and(|q| q.eq_ignore_ascii_case(t));
            if !known {
                *bad = true;
            }
        }
        return Expr::Column {
            table: None,
            name: name.to_ascii_lowercase(),
        };
    }
    crate::executor::map_expr_children(e, &mut |c| Ok(normalize_columns(c, default_q, quals, bad)))
        .unwrap_or_else(|_| e.clone())
}
