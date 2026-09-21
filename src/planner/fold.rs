//! Plan-time constant folding + no-op Filter elimination
//! (PostgreSQL's `eval_const_expressions` borrowed, conservatively).
//!
//! What folds:
//! - Binary arithmetic / comparison / logical / concat / bitwise /
//!   shift operators when BOTH operands are literals — evaluated with
//!   `apply_binary`, the exact runtime semantics (int overflow promotes
//!   to real, division by zero yields NULL, 3-valued logic on NULL).
//! - Partial logical folds: `TRUE AND x` → `x`, `FALSE OR x` → `x`,
//!   `FALSE AND pure(x)` → `FALSE`, `TRUE OR pure(x)` → `TRUE`
//!   (short-circuit folds require the DISCARDED side to be pure — no
//!   function calls (side effects), no subqueries (cost)).
//! - Unary `-`/`+`/`NOT`/`~` on literals; `IS NULL` / `IS` on literals.
//! - `coalesce`/`ifnull` argument lists pruned of leading NULL literals
//!   and collapsed when a leading non-NULL literal exists.
//!
//! What deliberately does NOT fold (documented):
//! - Function calls other than coalesce/ifnull (user functions may have
//!   side effects; `random()` etc. must stay per-row).
//! - Subqueries, CASE, BETWEEN, IN, LIKE/GLOB/REGEXP, CAST (its
//!   conversion depends on the connection's text encoding).
//! - `->`/`->>`/`@@`/`<->` (JSON/FTS/KNN operators can raise on malformed
//!   operands — folding would move or hide the error).
//!
//! `Filter(TRUE)` wrappers are dropped entirely (their input passes
//! through unchanged); `Filter(FALSE/NULL)` wrappers stay (the per-row
//! evaluation is already cheap and preserves error behavior inside the
//! predicate).
//!
//! The pass is idempotent and parameter-independent, so it is safe to
//! run on plans cached across executions with different bindings.

use crate::executor::apply_binary;
use crate::planner::plan::Plan;
use crate::sql::ast::{BinaryOp, Expr, UnaryOp};
use crate::types::Value;

/// True when the expression has no observable side effects and no
/// execution cost that a fold could duplicate or skip: no function
/// calls, no subqueries, no RAISE. (Column refs and parameters are fine
/// — a discarded operand's column read is not observable.)
fn expr_is_pure(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Column { .. } => true,
        Expr::Binary { left, right, .. } => expr_is_pure(left) && expr_is_pure(right),
        Expr::Unary { expr, .. } => expr_is_pure(expr),
        Expr::Between {
            expr, low, high, ..
        } => expr_is_pure(expr) && expr_is_pure(low) && expr_is_pure(high),
        Expr::In { expr, source, .. } => {
            expr_is_pure(expr)
                && match source {
                    crate::sql::ast::InSource::List(list) => list.iter().all(expr_is_pure),
                    crate::sql::ast::InSource::Subquery(_)
                    | crate::sql::ast::InSource::Table(_) => false,
                }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            expr_is_pure(expr)
                && expr_is_pure(pattern)
                && escape.as_ref().map(|e| expr_is_pure(e)).unwrap_or(true)
        }
        Expr::IsNull { expr, .. } => expr_is_pure(expr),
        Expr::Is { left, right, .. } => expr_is_pure(left) && expr_is_pure(right),
        Expr::Cast { expr, .. } => expr_is_pure(expr),
        Expr::Collate { expr, .. } => expr_is_pure(expr),
        Expr::Row(items) => items.iter().all(expr_is_pure),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_ref().map(|o| expr_is_pure(o)).unwrap_or(true)
                && whens
                    .iter()
                    .all(|(w, t)| expr_is_pure(w) && expr_is_pure(t))
                && else_.as_ref().map(|e| expr_is_pure(e)).unwrap_or(true)
        }
        // Function calls (side effects), subqueries (cost + possible
        // errors), RAISE (observable control flow).
        Expr::Function { .. } | Expr::Subquery(_) | Expr::Exists(_) | Expr::Raise { .. } => false,
    }
}

fn literal_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        other => other.is_truthy(),
    }
}

/// Fold one expression in place (bottom-up).
pub(crate) fn fold_expr(e: &mut Expr) {
    // Children first.
    match e {
        Expr::Binary { left, right, .. } => {
            fold_expr(left);
            fold_expr(right);
        }
        Expr::Unary { expr, .. } => fold_expr(expr),
        Expr::Between {
            expr, low, high, ..
        } => {
            fold_expr(expr);
            fold_expr(low);
            fold_expr(high);
        }
        Expr::In { expr, source, .. } => {
            fold_expr(expr);
            if let crate::sql::ast::InSource::List(list) = source {
                for item in std::sync::Arc::make_mut(list).iter_mut() {
                    fold_expr(item);
                }
            }
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            fold_expr(expr);
            fold_expr(pattern);
            if let Some(esc) = escape {
                fold_expr(esc);
            }
        }
        Expr::IsNull { expr, .. } => fold_expr(expr),
        Expr::Is { left, right, .. } => {
            fold_expr(left);
            fold_expr(right);
        }
        Expr::Function { args, filter, .. } => {
            for a in args.iter_mut() {
                fold_expr(a);
            }
            if let Some(f) = filter {
                fold_expr(f);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            if let Some(o) = operand {
                fold_expr(o);
            }
            for (w, t) in whens.iter_mut() {
                fold_expr(w);
                fold_expr(t);
            }
            if let Some(el) = else_ {
                fold_expr(el);
            }
        }
        Expr::Row(items) => {
            for item in items.iter_mut() {
                fold_expr(item);
            }
        }
        Expr::Cast { expr, .. } => fold_expr(expr),
        Expr::Collate { expr, .. } => fold_expr(expr),
        Expr::Raise { message, .. } => {
            if let Some(m) = message {
                fold_expr(m);
            }
        }
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Column { .. } => {}
        Expr::Subquery(_) | Expr::Exists(_) => {}
    }

    // Then this node.
    match e {
        Expr::Binary { op, left, right } => {
            if let Some(replacement) = fold_binary(*op, left, right) {
                *e = replacement;
            }
        }
        Expr::Unary { op, expr } => {
            if let Some(replacement) = fold_unary(*op, expr) {
                *e = replacement;
            }
        }
        Expr::IsNull { expr, negated } => {
            if let Expr::Literal(v) = &**expr {
                let is_null = v.is_null();
                *e = Expr::Literal(Value::Integer(if is_null ^ *negated { 1 } else { 0 }));
            }
        }
        Expr::Is {
            left,
            right,
            negated,
        } => {
            if let (Expr::Literal(l), Expr::Literal(r)) = (&**left, &**right) {
                let equal = if l.is_null() && r.is_null() {
                    true
                } else if l.is_null() || r.is_null() {
                    false
                } else {
                    l.cmp(r) == std::cmp::Ordering::Equal
                };
                *e = Expr::Literal(Value::Integer(if equal ^ *negated { 1 } else { 0 }));
            }
        }
        Expr::Function { name, args, .. } => {
            if let Some(replacement) = fold_coalesce(name, args) {
                *e = replacement;
            }
        }
        _ => {}
    }
}

/// Fold a Binary node (children already folded). Returns the folded
/// replacement when a fold applies. FULL-LITERAL folds only — exact
/// runtime semantics via `apply_binary`, safe in every context.
fn fold_binary(op: BinaryOp, left: &mut Expr, right: &mut Expr) -> Option<Expr> {
    use BinaryOp::*;
    // Operators that can RAISE (JSON/FTS/KNN) never fold.
    if matches!(op, Arrow | ArrowText | FtsMatch | Distance) {
        return None;
    }
    if let (Expr::Literal(l), Expr::Literal(r)) = (&*left, &*right) {
        return Some(Expr::Literal(apply_binary(op, l, r)));
    }
    None
}

fn lit_truthy(e: &Expr) -> bool {
    matches!(e, Expr::Literal(v) if literal_truthy(v))
}

/// "Definitely false" — a literal that is neither NULL (unknown) nor
/// truthy. NULL literals are NEITHER truthy NOR definitely-false:
/// three-valued logic keeps `NULL AND x` / `NULL OR x` unfoldable.
fn lit_false(e: &Expr) -> bool {
    matches!(e, Expr::Literal(v) if !v.is_null() && !v.is_truthy())
}

/// Boolean-context fold (value consumed ONLY as a truth value: filter
/// predicates, residuals, join conditions). After the ordinary fold, the
/// TOP-LEVEL node may simplify further:
/// - `TRUE AND x` / `x AND TRUE` -> `x`
/// - `FALSE OR x` / `x OR FALSE` -> `x`
/// - `FALSE AND pure(x)` -> `FALSE`   (canonical 0 — AND/OR yield 0/1/NULL)
/// - `TRUE OR pure(x)`   -> `TRUE`    (canonical 1)
///
/// The discarded side must be pure (no function calls, no subqueries).
pub(crate) fn fold_boolean_expr(e: &mut Expr) {
    fold_expr(e);
    if let Expr::Binary { op, left, right } = e {
        match op {
            BinaryOp::And => {
                if lit_truthy(left) {
                    let x = std::mem::replace(right.as_mut(), Expr::Literal(Value::Null));
                    *e = x;
                } else if lit_truthy(right) {
                    let x = std::mem::replace(left.as_mut(), Expr::Literal(Value::Null));
                    *e = x;
                } else if (lit_false(left) && expr_is_pure(right))
                    || (lit_false(right) && expr_is_pure(left))
                {
                    *e = Expr::Literal(Value::Integer(0));
                }
            }
            BinaryOp::Or => {
                if lit_false(left) {
                    let x = std::mem::replace(right.as_mut(), Expr::Literal(Value::Null));
                    *e = x;
                } else if lit_false(right) {
                    let x = std::mem::replace(left.as_mut(), Expr::Literal(Value::Null));
                    *e = x;
                } else if (lit_truthy(left) && expr_is_pure(right))
                    || (lit_truthy(right) && expr_is_pure(left))
                {
                    *e = Expr::Literal(Value::Integer(1));
                }
            }
            _ => {}
        }
    }
}

fn fold_unary(op: UnaryOp, expr: &mut Expr) -> Option<Expr> {
    if let Expr::Literal(v) = &*expr {
        match (op, v) {
            (UnaryOp::Neg, Value::Integer(i)) => {
                return Some(Expr::Literal(match i.checked_neg() {
                    Some(n) => Value::Integer(n),
                    // -i64::MIN = +2^63 has no i64 form: promote to REAL
                    // (SQLite; `SELECT -(-9223372036854775808)` is
                    // 9.223372036854776e18). wrapping_neg kept the
                    // UN-negated value — the minus sign silently vanished.
                    None => Value::Real(-(*i as f64)),
                }));
            }
            (UnaryOp::Neg, Value::Real(f)) => {
                return Some(Expr::Literal(Value::Real(-f)));
            }
            (UnaryOp::Pos, _) => {
                return Some(Expr::Literal(v.clone()));
            }
            (UnaryOp::Not, Value::Null) => {
                return Some(Expr::Literal(Value::Null));
            }
            (UnaryOp::Not, other) => {
                return Some(Expr::Literal(Value::Integer(if other.is_truthy() {
                    0
                } else {
                    1
                })));
            }
            (UnaryOp::BitNot, Value::Integer(i)) => {
                return Some(Expr::Literal(Value::Integer(!i)));
            }
            (UnaryOp::BitNot, Value::Null) => {
                return Some(Expr::Literal(Value::Null));
            }
            _ => {}
        }
    }
    None
}

/// coalesce/ifnull: leading NULL literals drop; a leading non-NULL
/// literal collapses the whole call.
fn fold_coalesce(name: &str, args: &mut [Expr]) -> Option<Expr> {
    if !name.eq_ignore_ascii_case("coalesce") && !name.eq_ignore_ascii_case("ifnull") {
        return None;
    }
    // First non-literal argument stops the fold (later args may be
    // observable only through errors — keep them).
    let mut leading: Vec<Value> = Vec::new();
    for a in args.iter() {
        match a {
            Expr::Literal(v) if v.is_null() => leading.push(Value::Null),
            Expr::Literal(v) => {
                // First non-NULL literal wins outright.
                return Some(Expr::Literal(v.clone()));
            }
            _ => break,
        }
    }
    if leading.is_empty() {
        return None; // first arg already non-literal
    }
    if leading.len() == args.len() {
        // All NULL literals.
        return Some(Expr::Literal(Value::Null));
    }
    // Partial list (leading NULLs then a non-literal): conservative —
    // keep the call. The runtime coalesce already prunes cheaply, and
    // splicing a slice in place is not possible without the owning Vec.
    None
}

/// Fold every expression in the plan tree and drop `Filter(TRUE)`
/// wrappers. Mirrors the rowid-rewrite walker's coverage. Predicates
/// (scan predicates, residuals, join conditions, FILTER clauses) fold as
/// boolean roots — the identity simplifications apply there; everything
/// else gets the context-safe full-literal folds only.
pub(crate) fn fold_constants_in_plan(plan: &mut Plan) {
    // Phase 1: boolean-context roots (identity simplifications allowed —
    // the value is consumed purely as a truth value).
    let bool_roots: Vec<&mut Expr> = match plan {
        Plan::Scan { predicate, .. } => predicate.iter_mut().collect(),
        Plan::RowidIn { residual, .. } => residual.iter_mut().collect(),
        Plan::RowidRange { residual, .. } => residual.iter_mut().collect(),
        Plan::IndexIn { residual, .. } => residual.iter_mut().collect(),
        Plan::IndexRange { residual, .. } => residual.iter_mut().collect(),
        Plan::Filter { predicate, .. } => vec![predicate],
        Plan::Join { condition, .. } => condition.iter_mut().collect(),
        Plan::Aggregate { aggregates, .. } => aggregates
            .iter_mut()
            .filter_map(|a| a.filter.as_mut())
            .collect(),
        Plan::Window { windows, .. } => windows
            .iter_mut()
            .filter_map(|w| w.filter.as_mut())
            .collect(),
        Plan::Update { from, .. } => from
            .as_mut()
            .and_then(|uf| uf.where_clause.as_mut())
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    for e in bool_roots {
        fold_boolean_expr(e);
    }

    // Phase 2: recurse into children + value-context folds.
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
        Plan::InvertedIndexScan {
            tsquery_expr,
            residual,
            ..
        } => residual
            .iter_mut()
            .chain(std::iter::once(tsquery_expr))
            .collect(),
        Plan::SpatialIndexScan {
            point_expr,
            radius_expr,
            residual,
            ..
        } => residual
            .iter_mut()
            .chain(std::iter::once(point_expr))
            .chain(std::iter::once(radius_expr))
            .collect(),
        Plan::SpatialKnn {
            point_expr,
            limit_expr,
            residual,
            ..
        } => residual
            .iter_mut()
            .chain(std::iter::once(point_expr))
            .chain(std::iter::once(limit_expr))
            .collect(),
        Plan::Values { rows } => rows.iter_mut().flatten().collect(),
        Plan::Filter { input, .. } => {
            fold_constants_in_plan(input);
            Vec::new() // predicate already folded as a boolean root
        }
        Plan::Project { input, columns } => {
            fold_constants_in_plan(input);
            columns.iter_mut().map(|c| &mut c.expr).collect()
        }
        Plan::Sort { input, terms } => {
            fold_constants_in_plan(input);
            terms.iter_mut().map(|t| &mut t.expr).collect()
        }
        Plan::Limit {
            input,
            count,
            offset,
        } => {
            fold_constants_in_plan(input);
            vec![count, offset]
        }
        Plan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            fold_constants_in_plan(input);
            let mut out: Vec<&mut Expr> = group_by.iter_mut().collect();
            for a in aggregates.iter_mut() {
                if let Some(arg) = a.arg.as_mut() {
                    out.push(arg);
                }
            }
            out
        }
        Plan::Window { input, windows } => {
            fold_constants_in_plan(input);
            let mut out: Vec<&mut Expr> = Vec::new();
            for w in windows.iter_mut() {
                if let Some(a) = w.arg.as_mut() {
                    out.push(a);
                }
                for a in w.extra_args.iter_mut() {
                    out.push(a);
                }
                for p in w.partition_by.iter_mut() {
                    out.push(p);
                }
                for t in w.order_by.iter_mut() {
                    out.push(&mut t.expr);
                }
            }
            out
        }
        Plan::Join {
            left,
            right,
            condition,
            ..
        } => {
            fold_constants_in_plan(left);
            fold_constants_in_plan(right);
            condition.iter_mut().collect()
        }
        Plan::IndexNestedLoopJoin { outer, .. } => {
            fold_constants_in_plan(outer);
            Vec::new()
        }
        Plan::Subquery { plan } => {
            fold_constants_in_plan(plan);
            Vec::new()
        }
        Plan::CteRows { .. } => Vec::new(),
        Plan::TableFunction { args, .. } => args.iter_mut().collect(),
        Plan::Distinct { input } => {
            fold_constants_in_plan(input);
            Vec::new()
        }
        Plan::Union { left, right, .. } => {
            fold_constants_in_plan(left);
            fold_constants_in_plan(right);
            Vec::new()
        }
        Plan::Intersect { left, right } => {
            fold_constants_in_plan(left);
            fold_constants_in_plan(right);
            Vec::new()
        }
        Plan::Except { left, right } => {
            fold_constants_in_plan(left);
            fold_constants_in_plan(right);
            Vec::new()
        }
        Plan::Insert { source, .. } => {
            fold_constants_in_plan(source);
            Vec::new()
        }
        Plan::Update {
            source,
            assignments,
            from,
            ..
        } => {
            fold_constants_in_plan(source);
            let out: Vec<&mut Expr> = assignments.iter_mut().map(|(_, e)| e).collect();
            if let Some(uf) = from {
                fold_constants_in_plan(&mut uf.plan);
            }
            out
        }
        Plan::Delete { source, .. } => {
            fold_constants_in_plan(source);
            Vec::new()
        }
    };
    for e in exprs {
        fold_expr(e);
    }

    // Phase 3: no-op Filter elimination (post boolean fold).
    if let Plan::Filter { input, predicate } = plan {
        if matches!(predicate, Expr::Literal(v) if literal_truthy(v)) {
            // No-op filter: replace the whole node with its input.
            let inner = std::mem::replace(&mut **input, Plan::Values { rows: Vec::new() });
            *plan = inner;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(i: i64) -> Expr {
        Expr::Literal(Value::Integer(i))
    }

    /// Literal value of a folded expression (None when not a literal).
    fn lit(e: &Expr) -> Option<Value> {
        match e {
            Expr::Literal(v) => Some(v.clone()),
            _ => None,
        }
    }

    fn col(name: &str) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }

    fn impure() -> Expr {
        Expr::Function {
            name: "random".into(),
            distinct: false,
            args: Vec::new(),
            filter: None,
            over: None,
        }
    }

    fn add(l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op: BinaryOp::Add,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    fn andb(l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    fn orb(l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            op: BinaryOp::Or,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    #[test]
    fn arithmetic_folds() {
        let mut e = add(int(2), int(3));
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(5)));

        // nested bottom-up: (2+3)*4
        let mut e = Expr::Binary {
            op: BinaryOp::Mul,
            left: Box::new(add(int(2), int(3))),
            right: Box::new(int(4)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(20)));

        // division by zero -> NULL (SQLite)
        let mut e = Expr::Binary {
            op: BinaryOp::Div,
            left: Box::new(int(1)),
            right: Box::new(int(0)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Null));

        // integer overflow promotes to real
        let mut e = add(int(i64::MAX), int(1));
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Real(9223372036854775808.0)));

        // comparisons fold
        let mut e = Expr::Binary {
            op: BinaryOp::Lt,
            left: Box::new(int(2)),
            right: Box::new(int(3)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));
    }

    #[test]
    fn logical_partial_folds_boolean_context() {
        // TRUE AND x -> x (boolean roots only)
        let mut e = andb(int(1), col("x"));
        fold_boolean_expr(&mut e);
        assert!(matches!(e, Expr::Column { .. }), "got {e:?}");

        // FALSE AND x -> FALSE (x pure)
        let mut e = andb(int(0), col("x"));
        fold_boolean_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(0)));

        // FALSE OR x -> x
        let mut e = orb(int(0), col("x"));
        fold_boolean_expr(&mut e);
        assert!(matches!(e, Expr::Column { .. }), "got {e:?}");

        // TRUE OR x -> TRUE
        let mut e = orb(int(1), col("x"));
        fold_boolean_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));

        // NULL AND x stays (x could be false — 3VL)
        let mut e = andb(Expr::Literal(Value::Null), col("x"));
        fold_boolean_expr(&mut e);
        assert!(matches!(e, Expr::Binary { .. }), "got {e:?}");

        // NULL OR x stays
        let mut e = orb(Expr::Literal(Value::Null), col("x"));
        fold_boolean_expr(&mut e);
        assert!(matches!(e, Expr::Binary { .. }), "got {e:?}");

        // FALSE AND random() stays (impure side effect)
        let mut e = andb(int(0), impure());
        fold_boolean_expr(&mut e);
        assert!(matches!(e, Expr::Binary { .. }), "got {e:?}");

        // VALUE CONTEXT: identity folds must NOT apply — SQLite's AND
        // yields 0/1/NULL, never the operand itself.
        let mut e = andb(int(1), int(5));
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)), "SELECT 1 AND 5 is 1");
        let mut e = orb(int(0), int(5));
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)), "SELECT 0 OR 5 is 1");
        // and in a projection the non-literal operand survives verbatim:
        let mut e = andb(int(1), col("x"));
        fold_expr(&mut e);
        assert!(matches!(e, Expr::Binary { .. }), "got {e:?}");
    }

    #[test]
    fn is_null_and_coalesce_fold() {
        let mut e = Expr::IsNull {
            expr: Box::new(Expr::Literal(Value::Null)),
            negated: false,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));

        let mut e = Expr::IsNull {
            expr: Box::new(int(5)),
            negated: false,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(0)));

        // coalesce(NULL, NULL) -> NULL
        let mut e = Expr::Function {
            name: "coalesce".into(),
            distinct: false,
            args: vec![Expr::Literal(Value::Null), Expr::Literal(Value::Null)],
            filter: None,
            over: None,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Null));

        // coalesce(NULL, 7, x) -> 7
        let mut e = Expr::Function {
            name: "coalesce".into(),
            distinct: false,
            args: vec![Expr::Literal(Value::Null), int(7), col("x")],
            filter: None,
            over: None,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(7)));

        // ifnull(NULL, NULL) -> NULL
        let mut e = Expr::Function {
            name: "ifnull".into(),
            distinct: false,
            args: vec![Expr::Literal(Value::Null), Expr::Literal(Value::Null)],
            filter: None,
            over: None,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Null));
    }

    #[test]
    fn unary_folds() {
        let mut e = Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(int(5)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(-5)));

        let mut e = Expr::Unary {
            op: UnaryOp::BitNot,
            expr: Box::new(int(0)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(-1)));

        let mut e = Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(int(0)),
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));
    }

    #[test]
    fn is_folds() {
        // literal IS literal
        let mut e = Expr::Is {
            left: Box::new(int(5)),
            right: Box::new(int(5)),
            negated: false,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));

        // NULL IS NULL -> true
        let mut e = Expr::Is {
            left: Box::new(Expr::Literal(Value::Null)),
            right: Box::new(Expr::Literal(Value::Null)),
            negated: false,
        };
        fold_expr(&mut e);
        assert_eq!(lit(&e), Some(Value::Integer(1)));
    }
}
