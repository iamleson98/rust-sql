//! NATURAL and USING joins — SQLite's `sqlite3ProcessJoin`.
//!
//! `a NATURAL JOIN b` joins on every column name the two sides share;
//! `a JOIN b USING (k)` on `a.k = b.k`. In both, the shared column
//! appears ONCE in `*` (the left copy — or `coalesce(left, right)` for a
//! RIGHT / FULL join, whose left copy may be the NULL one). The planner
//! rewrites such a FROM clause up front: the constraint becomes a
//! qualified ON term and every `*` becomes the explicit column list.

use super::Planner;
use crate::sql::ast::*;

/// One FROM atom as the rewrite sees it.
struct Source {
    qualifier: String,
    columns: Vec<String>,
    /// Per column: what `*` emits for it (`None` = the right copy of a
    /// USING column, which `*` skips).
    star: Vec<Option<Expr>>,
}

impl Planner<'_> {
    /// The rewritten SELECT, or `None` when the FROM has no NATURAL /
    /// USING join (or one whose sides' columns are not statically known
    /// — it then plans as before).
    pub(super) fn resolve_using_joins(&self, s: &SimpleSelect) -> Option<SimpleSelect> {
        let from = s.from.as_ref()?;
        let table_star = matches!(from, TableExpression::Join { .. })
            && s.columns
                .iter()
                .any(|c| matches!(c, ResultColumn::TableStar(_)));
        if !has_using(from) && !table_star {
            return None;
        }
        let mut sources = Vec::new();
        let mut coalesced = Vec::new();
        let new_from = self.rewrite_from(from, &mut sources, &mut coalesced)?;
        let mut out = s.clone();
        out.from = Some(new_from);
        let plain = |src: &Source, name: &str| Expr::Column {
            table: Some(src.qualifier.clone()),
            name: name.to_string(),
        };
        let mut cols = Vec::with_capacity(out.columns.len());
        for c in std::mem::take(&mut out.columns) {
            match c {
                ResultColumn::Star => {
                    for src in &sources {
                        for (name, e) in src.columns.iter().zip(&src.star) {
                            if let Some(e) = e {
                                cols.push(ResultColumn::Expr {
                                    expr: e.clone(),
                                    alias: Some(name.clone()),
                                });
                            }
                        }
                    }
                }
                // `b.*` lists ALL of b's columns (a merged one too).
                ResultColumn::TableStar(q) => {
                    let src = sources
                        .iter()
                        .find(|s| s.qualifier.eq_ignore_ascii_case(&q))?;
                    for name in &src.columns {
                        cols.push(ResultColumn::Expr {
                            expr: plain(src, name),
                            alias: Some(name.clone()),
                        });
                    }
                }
                other => cols.push(other),
            }
        }
        out.columns = cols;
        // RIGHT / FULL: a bare merged name means coalesce(left, right).
        if !coalesced.is_empty() {
            let sub = |e: &Expr| coalesce_bare(e, &coalesced);
            for c in out.columns.iter_mut() {
                if let ResultColumn::Expr { expr, .. } = c {
                    *expr = sub(expr);
                }
            }
            out.where_clause = out.where_clause.as_ref().map(sub);
            out.having = out.having.as_ref().map(sub);
            for g in out.group_by.iter_mut() {
                *g = sub(g);
            }
        }
        Some(out)
    }

    fn rewrite_from(
        &self,
        te: &TableExpression,
        out: &mut Vec<Source>,
        coalesced: &mut Vec<(String, Expr)>,
    ) -> Option<TableExpression> {
        let TableExpression::Join {
            left,
            right,
            join_type,
            constraint,
        } = te
        else {
            let (qualifier, columns) = self.atom_columns(te)?;
            let star = columns
                .iter()
                .map(|c| {
                    Some(Expr::Column {
                        table: Some(qualifier.clone()),
                        name: c.clone(),
                    })
                })
                .collect();
            out.push(Source {
                qualifier,
                columns,
                star,
            });
            return Some(te.clone());
        };
        let start_l = out.len();
        let new_left = self.rewrite_from(left, out, coalesced)?;
        let start_r = out.len();
        let new_right = self.rewrite_from(right, out, coalesced)?;
        let using: Vec<String> = match constraint {
            JoinConstraint::Using(cols) => cols.clone(),
            JoinConstraint::Natural => {
                let mut common: Vec<String> = Vec::new();
                for src in &out[start_r..] {
                    for c in &src.columns {
                        let in_left = out[start_l..start_r]
                            .iter()
                            .any(|l| l.columns.iter().any(|x| x.eq_ignore_ascii_case(c)));
                        if in_left && !common.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                            common.push(c.clone());
                        }
                    }
                }
                common
            }
            _ => {
                return Some(TableExpression::Join {
                    left: Box::new(new_left),
                    right: Box::new(new_right),
                    join_type: *join_type,
                    constraint: constraint.clone(),
                })
            }
        };
        let find = |range: std::ops::Range<usize>, out: &[Source], c: &str| {
            range.into_iter().find_map(|si| {
                // A column `*` already skips (an earlier USING's right
                // copy) is not the one this join merges.
                let src = &out[si];
                (0..src.columns.len())
                    .find(|&ci| src.columns[ci].eq_ignore_ascii_case(c) && src.star[ci].is_some())
                    .map(|ci| (si, ci))
            })
        };
        let mut on: Option<Expr> = None;
        for c in &using {
            // Unknown name: leave the join to the existing path, which
            // raises SQLite's error.
            let (li, lc) = find(start_l..start_r, out, c)?;
            let (ri, rc) = find(start_r..out.len(), out, c)?;
            let l = Expr::Column {
                table: Some(out[li].qualifier.clone()),
                name: out[li].columns[lc].clone(),
            };
            let r = Expr::Column {
                table: Some(out[ri].qualifier.clone()),
                name: out[ri].columns[rc].clone(),
            };
            out[ri].star[rc] = None;
            if matches!(join_type, JoinType::Right | JoinType::Full) {
                let merged = Expr::Function {
                    name: "coalesce".to_string(),
                    distinct: false,
                    args: vec![l.clone(), r.clone()],
                    filter: None,
                    over: None,
                    order_by: Vec::new(),
                };
                out[li].star[lc] = Some(merged.clone());
                coalesced.push((c.clone(), merged));
            }
            let term = Expr::Binary {
                op: BinaryOp::Eq,
                left: Box::new(l),
                right: Box::new(r),
            };
            on = Some(match on {
                Some(prev) => Expr::Binary {
                    op: BinaryOp::And,
                    left: Box::new(prev),
                    right: Box::new(term),
                },
                None => term,
            });
        }
        Some(TableExpression::Join {
            left: Box::new(new_left),
            right: Box::new(new_right),
            // NATURAL with no shared column is a cross join.
            join_type: if on.is_none() && *join_type == JoinType::Inner {
                JoinType::Cross
            } else {
                *join_type
            },
            constraint: on.map_or(JoinConstraint::None, JoinConstraint::On),
        })
    }

    /// A FROM atom's qualifier and `*` column names. `None` when either
    /// is not statically known (unaliased subqueries, virtual tables,
    /// table functions, attached tables, star bodies over joins).
    fn atom_columns(&self, te: &TableExpression) -> Option<(String, Vec<String>)> {
        match te {
            TableExpression::Table {
                name,
                schema,
                alias,
                ..
            } => {
                if schema.as_deref().is_some_and(|s| {
                    !s.eq_ignore_ascii_case("main") && !s.eq_ignore_ascii_case("temp")
                }) {
                    return None;
                }
                let q = alias.clone().unwrap_or_else(|| name.clone());
                if schema.is_none() {
                    if let Some((_, cols, _)) = self.effective_cte(&name.to_ascii_lowercase()) {
                        let names = cols
                            .iter()
                            .map(|c| {
                                c.rsplit_once('.')
                                    .map_or(c.as_str(), |(_, n)| n)
                                    .to_string()
                            })
                            .collect();
                        return Some((q, names));
                    }
                }
                if let Some(t) = self.catalog.get_table(name) {
                    if t.vtab.is_some() {
                        return None;
                    }
                    return Some((q, t.columns.iter().map(|c| c.name.clone()).collect()));
                }
                let v = self.catalog.get_view(name)?;
                let names = match &v.columns {
                    Some(c) => c.clone(),
                    None => super::subquery_atom_output_names(self.catalog, &v.select)?,
                };
                Some((q, names))
            }
            TableExpression::Subquery {
                select,
                alias,
                column_aliases,
            } => {
                let q = alias.clone()?;
                let names = match column_aliases {
                    Some(c) => c.clone(),
                    None => super::subquery_atom_output_names(self.catalog, select)?,
                };
                Some((q, names))
            }
            _ => None,
        }
    }
}

/// Replace each bare reference to a RIGHT / FULL merged column (outside
/// subqueries, which are their own scope) with its coalesce.
fn coalesce_bare(e: &Expr, merged: &[(String, Expr)]) -> Expr {
    if let Expr::Column { table: None, name } = e {
        if let Some((_, m)) = merged.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
            return m.clone();
        }
        return e.clone();
    }
    crate::executor::map_expr_children(e, &mut |c| Ok(coalesce_bare(c, merged)))
        .unwrap_or_else(|_| e.clone())
}

fn has_using(te: &TableExpression) -> bool {
    match te {
        TableExpression::Join {
            left,
            right,
            constraint,
            ..
        } => {
            matches!(
                constraint,
                JoinConstraint::Using(_) | JoinConstraint::Natural
            ) || has_using(left)
                || has_using(right)
        }
        _ => false,
    }
}
