//! Compiled predicates: WHERE clauses pre-resolved to column indices.
//!
//! The general `eval_row` path walks the AST per row and resolves every
//! column reference by NAME (a case-insensitive linear scan over the
//! row's column names) — ~60-120 ns per simple `col > ?` comparison, plus
//! the full-row materialization needed to give it a positional slice.
//!
//! `compile_predicate` resolves a predicate ONCE per statement into a
//! small tree of positional comparisons:
//!
//! - `col <op> literal` / `literal <op> col` / `col <op> ?param`
//! - AND / OR / NOT chains
//! - `col IS [NOT] NULL`
//! - `col BETWEEN lo AND hi` (literal/param bounds)
//! - `col IN (literal, ...)` / `NOT IN`
//! - `col LIKE 'literal%'` (literal pattern)
//!
//! Evaluation indexes directly into the selectively-decoded column slice
//! — no name lookups, no full-row expansion, no AST walk. Anything the
//! compiler doesn't recognize falls back to the general path unchanged.
//!
//! Comparison semantics are IDENTICAL to the general path: every leaf
//! comparison goes through `apply_binary` with the same `BinaryOp`.

use crate::executor::expr::apply_binary;
use crate::sql::ast::{BinaryOp, Expr};
use crate::types::Value;

/// A value on the right-hand side of a compiled comparison.
#[derive(Clone, Debug)]
pub(crate) enum PredValue {
    /// A table column, by index.
    Col(usize),
    /// A positional parameter (`?`, `?N` — numeric name).
    Param(usize),
    /// A literal constant.
    Literal(Value),
}

impl PredValue {
    #[inline]
    fn eval<'a>(&'a self, row: &'a [Value], positions: &[usize], params: &'a [Value]) -> &'a Value {
        match self {
            PredValue::Col(i) => {
                let pos = positions[*i];
                row.get(pos).unwrap_or(NULL_REF())
            }
            PredValue::Param(i) => params.get(*i).unwrap_or(NULL_REF()),
            PredValue::Literal(v) => v,
        }
    }
}

// A shared NULL singleton for the borrow-unfriendly cases above.
static NULL_VALUE: Value = Value::Null;
#[inline]
fn NULL_REF() -> &'static Value {
    &NULL_VALUE
}

/// A predicate compiled against a fixed column-position layout.
#[derive(Clone, Debug)]
pub(crate) enum CompiledPredicate {
    /// `lhs op rhs` — both sides positional.
    Cmp {
        lhs: PredValue,
        op: BinaryOp,
        rhs: PredValue,
    },
    /// `a AND b`
    And(Box<CompiledPredicate>, Box<CompiledPredicate>),
    /// `a OR b`
    Or(Box<CompiledPredicate>, Box<CompiledPredicate>),
    /// `NOT a`
    Not(Box<CompiledPredicate>),
    /// `col IS NULL` / `col IS NOT NULL`
    IsNull { col: usize, negated: bool },
    /// `col BETWEEN lo AND hi` / negated
    Between {
        col: usize,
        lo: PredValue,
        hi: PredValue,
        negated: bool,
    },
    /// `col IN (v1, v2, ...)` / `NOT IN` — literal or param members.
    InList {
        col: usize,
        vals: Vec<PredValue>,
        negated: bool,
    },
    /// `col LIKE pattern` / `NOT LIKE` (literal or param pattern).
    Like {
        col: usize,
        pattern: PredValue,
        negated: bool,
    },
}

/// Column indices referenced by the predicate (for building the selective
/// decode list).
pub(crate) fn compiled_columns(p: &CompiledPredicate, out: &mut Vec<usize>) {
    match p {
        CompiledPredicate::Cmp { lhs, rhs, .. } => {
            if let PredValue::Col(i) = lhs {
                out.push(*i);
            }
            if let PredValue::Col(i) = rhs {
                out.push(*i);
            }
        }
        CompiledPredicate::And(a, b) | CompiledPredicate::Or(a, b) => {
            compiled_columns(a, out);
            compiled_columns(b, out);
        }
        CompiledPredicate::Not(a) => compiled_columns(a, out),
        CompiledPredicate::IsNull { col, .. } => out.push(*col),
        CompiledPredicate::Between { col, lo, hi, .. } => {
            out.push(*col);
            if let PredValue::Col(i) = lo {
                out.push(*i);
            }
            if let PredValue::Col(i) = hi {
                out.push(*i);
            }
        }
        CompiledPredicate::InList { col, vals, .. } => {
            out.push(*col);
            for v in vals {
                if let PredValue::Col(i) = v {
                    out.push(*i);
                }
            }
        }
        CompiledPredicate::Like { col, .. } => out.push(*col),
    }
}

impl CompiledPredicate {
    /// Evaluate the predicate against a selectively-decoded row slice.
    /// `positions[i]` is the position of table column `i` within `row`
    /// (usize::MAX when absent — treated as NULL).
    #[inline]
    pub(crate) fn eval(&self, row: &[Value], positions: &[usize], params: &[Value]) -> bool {
        match self {
            CompiledPredicate::Cmp { lhs, op, rhs } => {
                let l = lhs.eval(row, positions, params);
                let r = rhs.eval(row, positions, params);
                apply_binary(*op, l, r).is_truthy()
            }
            CompiledPredicate::And(a, b) => a.eval(row, positions, params) && b.eval(row, positions, params),
            CompiledPredicate::Or(a, b) => a.eval(row, positions, params) || b.eval(row, positions, params),
            CompiledPredicate::Not(a) => !a.eval(row, positions, params),
            CompiledPredicate::IsNull { col, negated } => {
                let is_null = matches!(row.get(positions[*col]), None | Some(Value::Null));
                is_null != *negated
            }
            CompiledPredicate::Between { col, lo, hi, negated } => {
                // SQL three-valued logic, mirroring the general path
                // exactly: any NULL operand → result NULL → WHERE filters
                // the row out (true for BETWEEN *and* NOT BETWEEN).
                let v = row.get(positions[*col]).unwrap_or(NULL_REF());
                let l = lo.eval(row, positions, params);
                let h = hi.eval(row, positions, params);
                if matches!(v, Value::Null) || matches!(l, Value::Null) || matches!(h, Value::Null) {
                    return false;
                }
                let in_range = apply_binary(BinaryOp::GtEq, v, l).is_truthy()
                    && apply_binary(BinaryOp::LtEq, v, h).is_truthy();
                in_range != *negated
            }
            CompiledPredicate::InList { col, vals, negated } => {
                let v = row.get(positions[*col]).unwrap_or(NULL_REF());
                // SQL IN semantics: NULL never matches; NOT IN with any
                // NULL member yields NULL (not true). We mirror eval's
                // behavior conservatively: membership = any equality true;
                // negation inverts truthiness only when no NULLs involved.
                let mut found = false;
                let mut saw_null = matches!(v, Value::Null);
                for cand in vals {
                    let c = cand.eval(row, positions, params);
                    if matches!(c, Value::Null) {
                        saw_null = true;
                    } else if apply_binary(BinaryOp::Eq, v, c).is_truthy() {
                        found = true;
                        break;
                    }
                }
                if *negated {
                    if saw_null {
                        false // NOT IN with NULL members: never true
                    } else {
                        !found
                    }
                } else {
                    found
                }
            }
            CompiledPredicate::Like { col, pattern, negated } => {
                let v = row.get(positions[*col]).unwrap_or(NULL_REF());
                let p = pattern.eval(row, positions, params);
                let matched = crate::executor::expr::like_match(v, p, None, false);
                matched != *negated
            }
        }
    }
}

/// Bind a leaf expression (literal / positional parameter / bare column).
fn bind_leaf(e: &Expr, table: &crate::schema::Table, prefix: &str) -> Option<PredValue> {
    match e {
        Expr::Literal(v) => Some(PredValue::Literal(v.clone())),
        Expr::Parameter(p) => p.parse::<usize>().ok().map(PredValue::Param),
        Expr::Column { table: ref_t, name } => {
            let matches = ref_t
                .as_ref()
                .map(|t| t == &table.name || t == prefix)
                .unwrap_or(true);
            if matches {
                table.find_column(name).map(PredValue::Col)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn is_cmp_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Eq | BinaryOp::NotEq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
    )
}

/// Compile a WHERE predicate against a table. Returns None when any part
/// of the expression isn't a supported shape (caller falls back to the
/// general `eval_row` path — semantics unchanged).
pub(crate) fn compile_predicate(
    e: &Expr,
    table: &crate::schema::Table,
    prefix: &str,
) -> Option<CompiledPredicate> {
    match e {
        Expr::Binary { op, left, right } => match op {
            BinaryOp::And => Some(CompiledPredicate::And(
                Box::new(compile_predicate(left, table, prefix)?),
                Box::new(compile_predicate(right, table, prefix)?),
            )),
            BinaryOp::Or => Some(CompiledPredicate::Or(
                Box::new(compile_predicate(left, table, prefix)?),
                Box::new(compile_predicate(right, table, prefix)?),
            )),
            op if is_cmp_op(*op) => {
                let lhs = bind_leaf(left, table, prefix)?;
                let rhs = bind_leaf(right, table, prefix)?;
                // SQL comparison with NULL on either side is never true —
                // handled by apply_binary's semantics, so pass through.
                Some(CompiledPredicate::Cmp { lhs, op: *op, rhs })
            }
            _ => None,
        },
        Expr::Unary { op, expr } => {
            // NOT expr
            if matches!(op, crate::sql::ast::UnaryOp::Not) {
                Some(CompiledPredicate::Not(Box::new(compile_predicate(expr, table, prefix)?)))
            } else {
                None
            }
        }
        Expr::IsNull { expr, negated } => {
            if let Expr::Column { table: ref_t, name } = expr.as_ref() {
                let matches = ref_t
                    .as_ref()
                    .map(|t| t == &table.name || t == prefix)
                    .unwrap_or(true);
                if matches {
                    if let Some(col) = table.find_column(name) {
                        return Some(CompiledPredicate::IsNull { col, negated: *negated });
                    }
                }
            }
            None
        }
        Expr::Between { expr, low, high, negated } => {
            if let Expr::Column { table: ref_t, name } = expr.as_ref() {
                let matches = ref_t
                    .as_ref()
                    .map(|t| t == &table.name || t == prefix)
                    .unwrap_or(true);
                if matches {
                    if let Some(col) = table.find_column(name) {
                        let lo = bind_leaf(low, table, prefix)?;
                        let hi = bind_leaf(high, table, prefix)?;
                        return Some(CompiledPredicate::Between { col, lo, hi, negated: *negated });
                    }
                }
            }
            None
        }
        Expr::In { expr, source, negated } => {
            let crate::sql::ast::InSource::List(vals) = source else {
                return None;
            };
            if vals.is_empty() {
                return None;
            }
            let Expr::Column { table: ref_t, name } = expr.as_ref() else {
                return None;
            };
            let matches = ref_t
                .as_ref()
                .map(|t| t == &table.name || t == prefix)
                .unwrap_or(true);
            if !matches {
                return None;
            }
            let col = table.find_column(name)?;
            let mut bound = Vec::with_capacity(vals.len());
            for v in vals {
                bound.push(bind_leaf(v, table, prefix)?);
            }
            Some(CompiledPredicate::InList { col, vals: bound, negated: *negated })
        }
        Expr::Like { op: _, expr, pattern, escape, negated } => {
            // LIKE only when the pattern is a compile-time literal or a
            // positional parameter and there's no ESCAPE clause.
            if escape.is_some() {
                return None;
            }
            let Expr::Column { table: ref_t, name } = expr.as_ref() else {
                return None;
            };
            let matches = ref_t
                .as_ref()
                .map(|t| t == &table.name || t == prefix)
                .unwrap_or(true);
            if !matches {
                return None;
            }
            let col = table.find_column(name)?;
            let pat = bind_leaf(pattern, table, prefix)?;
            if matches!(pat, PredValue::Col(_)) {
                return None; // column-vs-column LIKE: general path
            }
            Some(CompiledPredicate::Like { col, pattern: pat, negated: *negated })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> crate::schema::Table {
        // Build via the public schema API to keep column metadata real.
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, val INTEGER, score REAL)";
        let stmt = crate::sql::parse(sql).unwrap();
        if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table { columns, constraints, .. }) = stmt {
            crate::schema::build_table("t", &columns, &constraints, 1, false, false, "CREATE TABLE t").unwrap()
        } else {
            panic!("not a create table")
        }
    }

    #[test]
    fn compile_simple_cmp() {
        let t = table();
        let e = Expr::Binary {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Column { table: None, name: "val".into() }),
            right: Box::new(Expr::Literal(Value::Integer(5000))),
        };
        let p = compile_predicate(&e, &t, "t").unwrap();
        // row slice: [val=6000] at position 0 for column index 2
        let row = vec![Value::Integer(6000)];
        let positions = {
            let mut pos = vec![usize::MAX; 4];
            pos[2] = 0;
            pos
        };
        assert!(p.eval(&row, &positions, &[]));
        let row2 = vec![Value::Integer(4000)];
        assert!(!p.eval(&row2, &positions, &[]));
    }

    #[test]
    fn compile_and_chain_params() {
        let t = table();
        let e = Expr::Binary {
            op: BinaryOp::And,
            left: Box::new(Expr::Binary {
                op: BinaryOp::Gt,
                left: Box::new(Expr::Column { table: None, name: "val".into() }),
                right: Box::new(Expr::Parameter("0".into())),
            }),
            right: Box::new(Expr::Binary {
                op: BinaryOp::Lt,
                left: Box::new(Expr::Column { table: None, name: "score".into() }),
                right: Box::new(Expr::Parameter("1".into())),
            }),
        };
        let p = compile_predicate(&e, &t, "t").unwrap();
        let row = vec![Value::Integer(6000), Value::Real(0.5)];
        let positions = {
            let mut pos = vec![usize::MAX; 4];
            pos[2] = 0;
            pos[3] = 1;
            pos
        };
        assert!(p.eval(&row, &positions, &[Value::Integer(5000), Value::Real(1.0)]));
        assert!(!p.eval(&row, &positions, &[Value::Integer(5000), Value::Real(0.1)]));
    }

    #[test]
    fn compile_between_in_null() {
        let t = table();
        let e = Expr::Between {
            expr: Box::new(Expr::Column { table: None, name: "val".into() }),
            low: Box::new(Expr::Literal(Value::Integer(1))),
            high: Box::new(Expr::Literal(Value::Integer(10))),
            negated: false,
        };
        let p = compile_predicate(&e, &t, "t").unwrap();
        let positions = {
            let mut pos = vec![usize::MAX; 4];
            pos[2] = 0;
            pos
        };
        assert!(p.eval(&[Value::Integer(5)], &positions, &[]));
        assert!(!p.eval(&[Value::Integer(50)], &positions, &[]));

        let n = Expr::IsNull { expr: Box::new(Expr::Column { table: None, name: "name".into() }), negated: false };
        let p2 = compile_predicate(&n, &t, "t").unwrap();
        let pos2 = {
            let mut pos = vec![usize::MAX; 4];
            pos[1] = 0;
            pos
        };
        assert!(p2.eval(&[Value::Null], &pos2, &[]));
        assert!(!p2.eval(&[Value::Text("x".into())], &pos2, &[]));
    }
}
