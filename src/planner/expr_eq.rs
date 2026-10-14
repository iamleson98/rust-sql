//! Structural expression equality for the planner's matching passes
//! (GROUP BY terms, aggregate dedupe, conjunct filtering). It decides
//! exactly what comparing the two `{:?}` renderings did — the planner's
//! old idiom — without allocating: the formatted compare ran at every
//! level of every rewrite recursion, and showed as a third of a simple
//! aggregate statement's planning time. Rare shapes (subqueries, window
//! specs, RAISE) still compare by their rendering.

use crate::sql::ast::*;
use crate::types::Value;

fn debug_eq<T: std::fmt::Debug>(a: &T, b: &T) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(x), Value::Integer(y)) => x == y,
        // Bit equality: `{:?}` tells -0.0 from 0.0, and renders NaN alike.
        (Value::Real(x), Value::Real(y)) => {
            x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
        }
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Blob(x), Value::Blob(y)) => x == y,
        _ => false,
    }
}

fn opt_eq(a: &Option<Box<Expr>>, b: &Option<Box<Expr>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => expr_eq(x, y),
        _ => false,
    }
}

fn list_eq(a: &[Expr], b: &[Expr]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| expr_eq(x, y))
}

/// `format!("{a:?}") == format!("{b:?}")`, structurally.
pub(crate) fn expr_eq(a: &Expr, b: &Expr) -> bool {
    use Expr as E;
    match (a, b) {
        (E::Literal(x), E::Literal(y)) => value_eq(x, y),
        (E::Parameter(x), E::Parameter(y)) => x == y,
        (
            E::Column {
                table: t1,
                name: n1,
            },
            E::Column {
                table: t2,
                name: n2,
            },
        ) => t1 == t2 && n1 == n2,
        (
            E::Binary {
                op: o1,
                left: l1,
                right: r1,
            },
            E::Binary {
                op: o2,
                left: l2,
                right: r2,
            },
        ) => o1 == o2 && expr_eq(l1, l2) && expr_eq(r1, r2),
        (E::Unary { op: o1, expr: e1 }, E::Unary { op: o2, expr: e2 }) => {
            o1 == o2 && expr_eq(e1, e2)
        }
        (
            E::Between {
                expr: e1,
                low: lo1,
                high: h1,
                negated: n1,
            },
            E::Between {
                expr: e2,
                low: lo2,
                high: h2,
                negated: n2,
            },
        ) => n1 == n2 && expr_eq(e1, e2) && expr_eq(lo1, lo2) && expr_eq(h1, h2),
        (
            E::In {
                expr: e1,
                source: s1,
                negated: n1,
            },
            E::In {
                expr: e2,
                source: s2,
                negated: n2,
            },
        ) => {
            n1 == n2
                && expr_eq(e1, e2)
                && match (s1, s2) {
                    (InSource::List(x), InSource::List(y)) => list_eq(x, y),
                    (InSource::Table(x), InSource::Table(y)) => x == y,
                    (x, y) => debug_eq(x, y),
                }
        }
        (
            E::Like {
                op: o1,
                expr: e1,
                pattern: p1,
                escape: es1,
                negated: n1,
            },
            E::Like {
                op: o2,
                expr: e2,
                pattern: p2,
                escape: es2,
                negated: n2,
            },
        ) => o1 == o2 && n1 == n2 && expr_eq(e1, e2) && expr_eq(p1, p2) && opt_eq(es1, es2),
        (
            E::IsNull {
                expr: e1,
                negated: n1,
            },
            E::IsNull {
                expr: e2,
                negated: n2,
            },
        ) => n1 == n2 && expr_eq(e1, e2),
        (
            E::Is {
                left: l1,
                right: r1,
                negated: n1,
            },
            E::Is {
                left: l2,
                right: r2,
                negated: n2,
            },
        ) => n1 == n2 && expr_eq(l1, l2) && expr_eq(r1, r2),
        (
            E::Function {
                name: n1,
                distinct: d1,
                args: a1,
                filter: f1,
                over: w1,
                order_by: ob1,
            },
            E::Function {
                name: n2,
                distinct: d2,
                args: a2,
                filter: f2,
                over: w2,
                order_by: ob2,
            },
        ) => {
            n1 == n2
                && d1 == d2
                && list_eq(a1, a2)
                && opt_eq(f1, f2)
                && match (w1, w2) {
                    (None, None) => true,
                    (Some(x), Some(y)) => debug_eq(x, y),
                    _ => false,
                }
                && ob1.len() == ob2.len()
                && ob1.iter().zip(ob2).all(|(x, y)| {
                    x.order == y.order && x.nulls == y.nulls && expr_eq(&x.expr, &y.expr)
                })
        }
        (
            E::Case {
                operand: o1,
                whens: w1,
                else_: e1,
            },
            E::Case {
                operand: o2,
                whens: w2,
                else_: e2,
            },
        ) => {
            opt_eq(o1, o2)
                && opt_eq(e1, e2)
                && w1.len() == w2.len()
                && w1
                    .iter()
                    .zip(w2)
                    .all(|((c1, r1), (c2, r2))| expr_eq(c1, c2) && expr_eq(r1, r2))
        }
        (E::Row(x), E::Row(y)) => list_eq(x, y),
        (
            E::Cast {
                expr: e1,
                type_name: t1,
            },
            E::Cast {
                expr: e2,
                type_name: t2,
            },
        ) => t1 == t2 && expr_eq(e1, e2),
        (
            E::Collate {
                expr: e1,
                collation: c1,
            },
            E::Collate {
                expr: e2,
                collation: c2,
            },
        ) => c1 == c2 && expr_eq(e1, e2),
        (E::Subquery(_), E::Subquery(_))
        | (E::Exists(_), E::Exists(_))
        | (E::Raise { .. }, E::Raise { .. }) => debug_eq(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::expr_eq;

    fn e(sql: &str) -> crate::sql::ast::Expr {
        match crate::sql::parser::parse(&format!("SELECT {sql}")).unwrap() {
            crate::sql::ast::Statement::Select(s) => match s.body {
                crate::sql::ast::SelectBody::Simple(s) => match &s.columns[0] {
                    crate::sql::ast::ResultColumn::Expr { expr, .. } => expr.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn agrees_with_the_debug_rendering() {
        let cases = [
            "a",
            "t.a",
            "a + 1",
            "a + 1.0",
            "-0.0",
            "0.0",
            "'x'",
            "x'00'",
            "?1",
            ":p",
            "a BETWEEN 1 AND 2",
            "a NOT IN (1, 2)",
            "a IN (SELECT 1)",
            "a LIKE 'x%' ESCAPE '!'",
            "a IS NULL",
            "a IS NOT b",
            "count(*)",
            "count(DISTINCT a)",
            "sum(a) FILTER (WHERE b)",
            "group_concat(a ORDER BY b DESC)",
            "row_number() OVER (ORDER BY a)",
            "CASE a WHEN 1 THEN 2 ELSE 3 END",
            "(a, b)",
            "(SELECT 1)",
            "EXISTS (SELECT 1)",
            "CAST(a AS TEXT)",
            "a COLLATE nocase",
            "lower(a) || upper(b)",
        ];
        for x in &cases {
            for y in &cases {
                let (a, b) = (e(x), e(y));
                assert_eq!(
                    expr_eq(&a, &b),
                    format!("{a:?}") == format!("{b:?}"),
                    "{x} vs {y}"
                );
            }
        }
    }
}
