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
use crate::sql::ast::{BinaryOp, Expr, LikeOp};
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
    /// A compiled ARITHMETIC operand (`a % 10`, `b + 1` — any BinaryOp
    /// chain over columns/literals/params). Needed so predicates like
    /// `WHERE a % 10 = 0` compile: the leaf-only compiler bails on them
    /// and the fused scan+filter degrades to full materialization + AST
    /// walks. Positions-aware (unlike CompiledExpr) so it composes with
    /// selectively-decoded row buffers.
    Expr(PredExpr),
}

/// A positions-aware arithmetic expression tree for predicate operands.
#[derive(Clone, Debug)]
pub(crate) enum PredExpr {
    Col(usize),
    Param(usize),
    Literal(Value),
    Unary(UnaryOp, Box<PredExpr>),
    Binary(BinaryOp, Box<PredExpr>, Box<PredExpr>),
}

impl PredExpr {
    #[inline]
    fn eval(&self, row: &[Value], positions: &[usize], params: &[Value]) -> Value {
        match self {
            PredExpr::Col(i) => positions
                .get(*i)
                .and_then(|&p| row.get(p))
                .cloned()
                .unwrap_or(Value::Null),
            PredExpr::Param(i) => params.get(*i).cloned().unwrap_or(Value::Null),
            PredExpr::Literal(v) => v.clone(),
            PredExpr::Unary(op, e) => {
                let v = e.eval(row, positions, params);
                crate::executor::expr::apply_unary(*op, &v)
            }
            PredExpr::Binary(op, l, r) => {
                let lv = l.eval(row, positions, params);
                let rv = r.eval(row, positions, params);
                apply_binary(*op, &lv, &rv)
            }
        }
    }

    /// Fast INTEGER-only evaluation: `Some(i64)` when the whole
    /// expression folds over INTEGER operands; `None` on any NULL /
    /// REAL / TEXT involvement, division by zero, or integer overflow —
    /// the caller then takes the general path, which preserves full
    /// SQLite semantics (NULL results, REAL promotion of overflowing
    /// integer arithmetic, mixed-type coercion). Mirrors `arith_checked`
    /// and the Div/Mod NULL rules exactly, but without the per-row Value
    /// clones + apply_binary dispatch that dominated filtered scans:
    /// `WHERE a % 10 = 0` paid ~100 ns/row for two full Value
    /// round-trips (measured in examples/probe_mixed_reads.rs); this
    /// path evaluates it as two i64 ops (~5 ns).
    #[inline]
    fn eval_int(&self, row: &[Value], positions: &[usize], params: &[Value]) -> Option<i64> {
        match self {
            PredExpr::Literal(Value::Integer(i)) => Some(*i),
            PredExpr::Param(i) => match params.get(*i) {
                Some(Value::Integer(n)) => Some(*n),
                _ => None,
            },
            PredExpr::Col(i) => match positions.get(*i).and_then(|&p| row.get(p)) {
                Some(Value::Integer(n)) => Some(*n),
                _ => None,
            },
            PredExpr::Unary(op, e) => {
                if matches!(op, crate::sql::ast::UnaryOp::Neg) {
                    e.eval_int(row, positions, params)?.checked_neg()
                } else {
                    None
                }
            }
            PredExpr::Binary(op, l, r) => {
                use crate::sql::ast::BinaryOp as B;
                let a = l.eval_int(row, positions, params)?;
                let b = r.eval_int(row, positions, params)?;
                match op {
                    B::Add => a.checked_add(b),
                    B::Sub => a.checked_sub(b),
                    B::Mul => a.checked_mul(b),
                    // SQLite: x / 0 is NULL (fallback); MIN / -1 promotes
                    // to REAL (checked_div None → fallback → REAL).
                    B::Div => {
                        if b == 0 {
                            None
                        } else {
                            a.checked_div(b)
                        }
                    }
                    // SQLite: x % 0 is NULL (fallback); MIN % -1 is 0.
                    B::Mod => {
                        if b == 0 {
                            None
                        } else if b == -1 {
                            Some(0)
                        } else {
                            Some(a % b)
                        }
                    }
                    _ => None,
                }
            }
            // Non-integer literal, or any shape the fast path declines.
            _ => None,
        }
    }
}

impl PredValue {
    #[inline]
    fn eval<'a>(
        &'a self,
        row: &'a [Value],
        positions: &[usize],
        params: &'a [Value],
    ) -> std::borrow::Cow<'a, Value> {
        use std::borrow::Cow;
        match self {
            PredValue::Col(i) => {
                let pos = positions[*i];
                Cow::Borrowed(row.get(pos).unwrap_or(null_ref()))
            }
            PredValue::Param(i) => Cow::Borrowed(params.get(*i).unwrap_or(null_ref())),
            PredValue::Literal(v) => Cow::Borrowed(v),
            PredValue::Expr(pe) => Cow::Owned(pe.eval(row, positions, params)),
        }
    }
}

// A shared NULL singleton for the borrow-unfriendly cases above.
static NULL_VALUE: Value = Value::Null;
#[inline]
/// Linear IN membership over `vals` (the general path): returns
/// (found, saw_null). Kept as a helper so the integer-set fast path can
/// fall back to it for cross-type values (huge/NaN reals).
fn in_linear(
    v: &Value,
    vals: &[PredValue],
    row: &[Value],
    positions: &[usize],
    params: &[Value],
) -> (bool, bool) {
    let mut found = false;
    let mut saw_null = matches!(v, Value::Null);
    for cand in vals {
        let c = cand.eval(row, positions, params);
        if matches!(&*c, Value::Null) {
            saw_null = true;
        } else if apply_binary(BinaryOp::Eq, v, &c).is_truthy() {
            found = true;
            break;
        }
    }
    (found, saw_null)
}

fn null_ref() -> &'static Value {
    &NULL_VALUE
}

/// Resolve a predicate operand to a raw `i64` when it is — or points at —
/// an INTEGER. The dominant predicate shape over integer tables
/// (`WHERE a BETWEEN ? AND ?`, `id = ?`) then compares raw ints in the
/// fused scan loop instead of the generic Value machinery (NaN probe,
/// collation dispatch, Value construction per comparison). NULL / REAL /
/// TEXT / BLOB / arithmetic expressions return `None` and the caller
/// takes the general path with full SQLite semantics.
#[inline]
fn pred_int(pv: &PredValue, row: &[Value], positions: &[usize], params: &[Value]) -> Option<i64> {
    match pv {
        PredValue::Literal(Value::Integer(i)) => Some(*i),
        PredValue::Param(i) => match params.get(*i) {
            Some(Value::Integer(n)) => Some(*n),
            _ => None,
        },
        PredValue::Col(i) => match positions.get(*i).and_then(|&p| row.get(p)) {
            Some(Value::Integer(n)) => Some(*n),
            _ => None,
        },
        _ => None,
    }
}

/// `pred_int` extended to arithmetic EXPRESSION operands: `WHERE
/// a % 10 = 0` / `WHERE id + 1 = ?` compile to `PredValue::Expr`, which
/// the leaf-only resolver declines. Any integer-only expression folds
/// here; everything else falls back to the general path.
#[inline]
fn resolve_int(
    pv: &PredValue,
    row: &[Value],
    positions: &[usize],
    params: &[Value],
) -> Option<i64> {
    match pv {
        PredValue::Expr(pe) => pe.eval_int(row, positions, params),
        other => pred_int(other, row, positions, params),
    }
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
        /// Prebuilt membership set when EVERY member is a compile-time
        /// INTEGER literal — the big-IN fast path. A 5000-member
        /// `id IN (...)` scan then pays one hash probe per row instead of
        /// a 5000-element linear walk (SQLite builds the same ephemeral
        /// structure for large IN lists). Integer-only: no collation or
        /// affinity subtleties; mixed-type / param members keep the linear
        /// path. `saw_null` is folded in (a literal NULL member can never
        /// match under SQL IN semantics but poisons NOT IN).
        int_set: Option<std::collections::HashSet<i64>>,
    },
    /// `col LIKE/GLOB pattern` / `NOT LIKE|GLOB ...` (literal or param
    /// pattern). `glob` selects the matcher: GLOB is case-sensitive with
    /// `*`/`?` wildcards, LIKE is case-insensitive with `%`/`_`.
    Like {
        col: usize,
        pattern: PredValue,
        negated: bool,
        glob: bool,
    },
    /// `col = 'text-literal'` — byte equality without Value construction.
    /// Only built when the literal is TEXT (numeric literals keep the
    /// generic Cmp so cross-type coercion stays exact).
    TextEq { col: usize, rhs: PredValue },
    /// `col LIKE '%needle%'` — pre-classified contains search. Only built
    /// for ASCII needles with no other wildcards and no negation (NOT
    /// LIKE keeps the general Like arm). The needle bytes are stored
    /// inline (small) so per-row work is one substring search.
    LikeSubstr { col: usize, needle: Vec<u8> },
}

/// Column indices referenced by the predicate (for building the selective
/// decode list).
impl PredExpr {
    /// Collect the table columns this expression references, so the
    /// fused scan can selective-decode exactly them (see
    /// `compiled_columns`). Without this, `PredValue::Expr` operands
    /// (`a % 10 = 0`) yielded an EMPTY wanted list — the scan fell back
    /// to full-row decode for EVERY scanned row, paying ~50-60 ns/row of
    /// unneeded column materialization.
    fn collect_columns(&self, out: &mut Vec<usize>) {
        match self {
            PredExpr::Col(i) => out.push(*i),
            PredExpr::Unary(_, e) => e.collect_columns(out),
            PredExpr::Binary(_, l, r) => {
                l.collect_columns(out);
                r.collect_columns(out);
            }
            PredExpr::Literal(_) | PredExpr::Param(_) => {}
        }
    }
}

/// Collect column indices referenced by a `PredValue` operand
/// (leaf or expression).
#[inline]
fn operand_columns(pv: &PredValue, out: &mut Vec<usize>) {
    match pv {
        PredValue::Col(i) => out.push(*i),
        PredValue::Expr(pe) => pe.collect_columns(out),
        _ => {}
    }
}

pub(crate) fn compiled_columns(p: &CompiledPredicate, out: &mut Vec<usize>) {
    match p {
        CompiledPredicate::Cmp { lhs, rhs, .. } => {
            operand_columns(lhs, out);
            operand_columns(rhs, out);
        }
        CompiledPredicate::And(a, b) | CompiledPredicate::Or(a, b) => {
            compiled_columns(a, out);
            compiled_columns(b, out);
        }
        CompiledPredicate::Not(a) => compiled_columns(a, out),
        CompiledPredicate::IsNull { col, .. } => out.push(*col),
        CompiledPredicate::Between { col, lo, hi, .. } => {
            out.push(*col);
            operand_columns(lo, out);
            operand_columns(hi, out);
        }
        CompiledPredicate::InList { col, vals, .. } => {
            out.push(*col);
            for v in vals {
                operand_columns(v, out);
            }
        }
        CompiledPredicate::Like { col, .. } => out.push(*col),
        CompiledPredicate::TextEq { col, rhs } => {
            out.push(*col);
            operand_columns(rhs, out);
        }
        CompiledPredicate::LikeSubstr { col, .. } => out.push(*col),
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
                // Fast path: INTEGER <op> INTEGER. The generic path pays
                // cmp_operand_missing + collation dispatch + Value
                // construction per comparison (2 per BETWEEN row) — the
                // dominant cost of the fused scan-filter loop. NULL,
                // REAL, TEXT, BLOB operands decline and take the general
                // path with full SQLite semantics.
                if is_cmp_op(*op) {
                    if let (Some(l), Some(r)) = (
                        resolve_int(lhs, row, positions, params),
                        resolve_int(rhs, row, positions, params),
                    ) {
                        return match op {
                            BinaryOp::Eq => l == r,
                            BinaryOp::NotEq => l != r,
                            BinaryOp::Lt => l < r,
                            BinaryOp::LtEq => l <= r,
                            BinaryOp::Gt => l > r,
                            _ => l >= r, // GtEq (is_cmp_op total)
                        };
                    }
                }
                let l = lhs.eval(row, positions, params);
                let r = rhs.eval(row, positions, params);
                apply_binary(*op, &l, &r).is_truthy()
            }
            CompiledPredicate::And(a, b) => {
                a.eval(row, positions, params) && b.eval(row, positions, params)
            }
            CompiledPredicate::Or(a, b) => {
                a.eval(row, positions, params) || b.eval(row, positions, params)
            }
            CompiledPredicate::Not(a) => !a.eval(row, positions, params),
            CompiledPredicate::IsNull { col, negated } => {
                let is_null = matches!(row.get(positions[*col]), None | Some(Value::Null));
                is_null != *negated
            }
            CompiledPredicate::Between {
                col,
                lo,
                hi,
                negated,
            } => {
                // Fast path: all-INTEGER BETWEEN (the dominant shape of
                // integer range scans) — see the Cmp arm for rationale.
                // NULL on any operand declines here and takes the general
                // path, which filters the row out (SQL 3VL).
                let v_i = match row.get(positions[*col]) {
                    Some(Value::Integer(x)) => Some(*x),
                    _ => None,
                };
                if let (Some(x), Some(lo_i), Some(hi_i)) = (
                    v_i,
                    resolve_int(lo, row, positions, params),
                    resolve_int(hi, row, positions, params),
                ) {
                    let in_range = x >= lo_i && x <= hi_i;
                    return in_range != *negated;
                }
                // SQL three-valued logic, mirroring the general path
                // exactly: any NULL operand → result NULL → WHERE filters
                // the row out (true for BETWEEN *and* NOT BETWEEN).
                let v = row.get(positions[*col]).unwrap_or(null_ref());
                let l = lo.eval(row, positions, params);
                let h = hi.eval(row, positions, params);
                if matches!(v, Value::Null)
                    || matches!(&*l, Value::Null)
                    || matches!(&*h, Value::Null)
                {
                    return false;
                }
                let in_range = apply_binary(BinaryOp::GtEq, v, &l).is_truthy()
                    && apply_binary(BinaryOp::LtEq, v, &h).is_truthy();
                in_range != *negated
            }
            CompiledPredicate::InList {
                col,
                vals,
                negated,
                int_set,
            } => {
                let v = row.get(positions[*col]).unwrap_or(null_ref());
                // SQL IN semantics: NULL never matches; NOT IN with any
                // NULL member yields NULL (not true). We mirror eval's
                // behavior conservatively: membership = any equality true;
                // negation inverts truthiness only when no NULLs involved.
                let (found, saw_null) = if let Some(set) = int_set {
                    // All-integer-literal fast path: one probe. Reals match
                    // numerically when exactly representable (1.0 in (1,2));
                    // a real outside the exact range falls to the linear
                    // path below for full cross-type semantics.
                    match v {
                        Value::Integer(i) => (set.contains(i), false),
                        Value::Real(x) => {
                            if x.is_finite()
                                && x.fract() == 0.0
                                && x.abs() < 9.007_199_254_740_992e15
                            {
                                (set.contains(&(*x as i64)), false)
                            } else {
                                in_linear(v, vals, row, positions, params)
                            }
                        }
                        // NULL value: never a member; NOT IN yields NULL
                        // (filtered) — saw_null = true mirrors the linear
                        // path's initial condition.
                        Value::Null => (false, true),
                        // TEXT/BLOB never equals an integer member.
                        _ => (false, false),
                    }
                } else {
                    in_linear(v, vals, row, positions, params)
                };
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
            CompiledPredicate::Like {
                col,
                pattern,
                negated,
                glob,
            } => {
                let v = row.get(positions[*col]).unwrap_or(null_ref());
                let p = pattern.eval(row, positions, params);
                // NULL operand -> NULL result -> row filtered out.
                if v.is_null() || p.is_null() {
                    return false;
                }
                let matched = if *glob {
                    crate::executor::expr::glob_match(v, &p)
                } else {
                    crate::executor::expr::like_match(v, &p, None, false)
                };
                matched != *negated
            }
            CompiledPredicate::TextEq { col, rhs } => {
                // TEXT equality without Value construction: byte compare
                // directly. Non-TEXT operands (INTEGER/REAL compare
                // numerically in SQLite, e.g. 5 = '5') decline to the
                // general path.
                let l = row.get(positions[*col]).unwrap_or(null_ref());
                let r = rhs.eval(row, positions, params);
                match (l, &*r) {
                    (Value::Text(a), Value::Text(b)) => a.as_bytes() == b.as_bytes(),
                    _ => apply_binary(BinaryOp::Eq, l, &r).is_truthy(),
                }
            }
            CompiledPredicate::LikeSubstr { col, needle } => {
                // Pre-classified `%needle%` (ASCII needle, no other
                // wildcards): byte-substring search with ASCII folding —
                // no per-row pattern classification, no Value dispatch.
                // Non-TEXT values decline (SQLite casts numerics to text
                // for LIKE; the general path handles that).
                let v = row.get(positions[*col]).unwrap_or(null_ref());
                let Value::Text(t) = v else {
                    let p = Value::Text(needle_text(needle));
                    return crate::executor::expr::like_match(v, &p, None, false);
                };
                crate::executor::expr::like_contains_bytes(t.as_bytes(), needle)
            }
        }
    }
}

/// Rebuild a Text from a leaked byte slice (LikeSubstr needle storage).
fn needle_text(b: &[u8]) -> crate::types::text::Text {
    crate::types::text::Text::new(unsafe { std::str::from_utf8_unchecked(b) })
}

/// Bind a leaf expression (literal / positional parameter / bare column).
/// Comparison-operand compiler: a leaf when possible (borrow-friendly
/// evaluation), otherwise a positions-aware ARITHMETIC expression
/// (`a % 10 = 0`, `b + 1 < c`). Leaf-only operands used to send such
/// predicates to the general AST-walk path — and, worse, to bail out of
/// the fused scan+filter entirely (full row materialization before the
/// filter). Falls back to None only for genuinely unsupported shapes.
fn bind_operand(e: &Expr, table: &crate::schema::Table, prefix: &str) -> Option<PredValue> {
    if let Some(leaf) = bind_leaf(e, table, prefix) {
        return Some(leaf);
    }
    compile_pred_expr(e, table, prefix).map(PredValue::Expr)
}

/// Positions-aware arithmetic expression compiler (see `PredExpr`).
/// AND/OR are excluded (short-circuit semantics belong to the predicate
/// level, not eager operand evaluation).
fn compile_pred_expr(e: &Expr, table: &crate::schema::Table, prefix: &str) -> Option<PredExpr> {
    match e {
        Expr::Literal(v) => Some(PredExpr::Literal(v.clone())),
        Expr::Parameter(p) => p.parse::<usize>().ok().map(PredExpr::Param),
        Expr::Column { table: ref_t, name } => {
            let matches = ref_t
                .as_ref()
                .map(|t| {
                    // Same scoping rule as bind_leaf: an alias REPLACES the
                    // table name.
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
                .unwrap_or(true);
            if matches {
                table.find_column(name).map(PredExpr::Col)
            } else {
                None
            }
        }
        Expr::Unary { op, expr } => {
            let inner = compile_pred_expr(expr, table, prefix)?;
            Some(PredExpr::Unary(*op, Box::new(inner)))
        }
        Expr::Binary { op, left, right } => {
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                return None;
            }
            let l = compile_pred_expr(left, table, prefix)?;
            let r = compile_pred_expr(right, table, prefix)?;
            Some(PredExpr::Binary(*op, Box::new(l), Box::new(r)))
        }
        _ => None,
    }
}

fn bind_leaf(e: &Expr, table: &crate::schema::Table, prefix: &str) -> Option<PredValue> {
    match e {
        Expr::Literal(v) => Some(PredValue::Literal(v.clone())),
        Expr::Parameter(p) => p.parse::<usize>().ok().map(PredValue::Param),
        // `-10` parses as Unary(Neg, Literal(10)). Fold it so predicates
        // with negative literal bounds (`a BETWEEN -10 AND -1`, `a > -5`)
        // COMPILE — an unfolded bound made the whole predicate fall back
        // to the unfused path (full row materialization + AST-walk filter
        // per row: 150 ns/row vs 17 ns/row, measured in
        // examples/probe_mixed_reads.rs).
        Expr::Unary {
            op: crate::sql::ast::UnaryOp::Neg,
            expr,
        } => match expr.as_ref() {
            Expr::Literal(v @ (Value::Integer(_) | Value::Real(_))) => {
                let folded = match v {
                    Value::Integer(i) if *i != i64::MIN => Value::Integer(-i),
                    Value::Real(x) => Value::Real(-x),
                    // i64::MIN: -MIN overflows; the parser emits
                    // Literal(MIN) directly for it, so this is
                    // unreachable in practice — decline to fold.
                    _ => return None,
                };
                Some(PredValue::Literal(folded))
            }
            _ => None,
        },
        Expr::Column { table: ref_t, name } => {
            let matches = ref_t
                .as_ref()
                .map(|t| {
                    // SQL scoping: an alias REPLACES the table name —
                    // `t.col` must NOT bind to a `FROM t t2` instance
                    // (otherwise a correlated reference to an outer
                    // un-aliased `t` is silently captured by the inner
                    // alias and compared against itself).
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
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
        BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
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
                let lhs = bind_operand(left, table, prefix)?;
                let rhs = bind_operand(right, table, prefix)?;
                // TEXT `=` fast path: `col = 'literal'` (either side)
                // with a TEXT literal compiles to byte equality — no
                // per-row Value construction, NaN probes, or collation
                // dispatch. Non-Eq operators and non-TEXT literals keep
                // the generic Cmp (cross-type coercion must stay exact).
                if *op == BinaryOp::Eq {
                    if let (PredValue::Col(c), PredValue::Literal(Value::Text(_))) = (&lhs, &rhs) {
                        return Some(CompiledPredicate::TextEq {
                            col: *c,
                            rhs: rhs.clone(),
                        });
                    }
                    if let (PredValue::Literal(Value::Text(_)), PredValue::Col(c)) = (&lhs, &rhs) {
                        return Some(CompiledPredicate::TextEq {
                            col: *c,
                            rhs: lhs.clone(),
                        });
                    }
                }
                // SQL comparison with NULL on either side is never true —
                // handled by apply_binary's semantics, so pass through.
                Some(CompiledPredicate::Cmp { lhs, op: *op, rhs })
            }
            _ => None,
        },
        Expr::Unary { op, expr } => {
            // NOT expr
            if matches!(op, crate::sql::ast::UnaryOp::Not) {
                Some(CompiledPredicate::Not(Box::new(compile_predicate(
                    expr, table, prefix,
                )?)))
            } else {
                None
            }
        }
        Expr::IsNull { expr, negated } => {
            if let Expr::Column { table: ref_t, name } = expr.as_ref() {
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        // SQL scoping: an alias REPLACES the table name —
                        // `t.col` must NOT bind to a `FROM t t2` instance
                        // (otherwise a correlated reference to an outer
                        // un-aliased `t` is silently captured by the inner
                        // alias and compared against itself).
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
                if matches {
                    if let Some(col) = table.find_column(name) {
                        return Some(CompiledPredicate::IsNull {
                            col,
                            negated: *negated,
                        });
                    }
                }
            }
            None
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            if let Expr::Column { table: ref_t, name } = expr.as_ref() {
                let matches = ref_t
                    .as_ref()
                    .map(|t| {
                        // SQL scoping: an alias REPLACES the table name —
                        // `t.col` must NOT bind to a `FROM t t2` instance
                        // (otherwise a correlated reference to an outer
                        // un-aliased `t` is silently captured by the inner
                        // alias and compared against itself).
                        if prefix == table.name {
                            t == &table.name || t == prefix
                        } else {
                            t == prefix
                        }
                    })
                    .unwrap_or(true);
                if matches {
                    if let Some(col) = table.find_column(name) {
                        let lo = bind_leaf(low, table, prefix)?;
                        let hi = bind_leaf(high, table, prefix)?;
                        return Some(CompiledPredicate::Between {
                            col,
                            lo,
                            hi,
                            negated: *negated,
                        });
                    }
                }
            }
            None
        }
        Expr::In {
            expr,
            source,
            negated,
        } => {
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
                .map(|t| {
                    // SQL scoping: an alias REPLACES the table name —
                    // `t.col` must NOT bind to a `FROM t t2` instance
                    // (otherwise a correlated reference to an outer
                    // un-aliased `t` is silently captured by the inner
                    // alias and compared against itself).
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
                .unwrap_or(true);
            if !matches {
                return None;
            }
            let col = table.find_column(name)?;
            let mut bound = Vec::with_capacity(vals.len());
            for v in vals {
                bound.push(bind_leaf(v, table, prefix)?);
            }
            // All-integer-literal members: prebuilt membership set (the
            // big-IN fast path — see the variant docs). Everything else
            // (params, columns, arithmetic members, mixed types) keeps
            // the linear walk with full cross-type semantics.
            let int_set = if bound.len() > 1
                && bound
                    .iter()
                    .all(|p| matches!(p, PredValue::Literal(Value::Integer(_))))
            {
                let mut s = std::collections::HashSet::with_capacity_and_hasher(
                    bound.len() * 2,
                    std::collections::hash_map::RandomState::new(),
                );
                for p in &bound {
                    if let PredValue::Literal(Value::Integer(i)) = p {
                        s.insert(*i);
                    }
                }
                Some(s)
            } else {
                None
            };
            Some(CompiledPredicate::InList {
                col,
                vals: bound,
                negated: *negated,
                int_set,
            })
        }
        Expr::Like {
            op,
            expr,
            pattern,
            escape,
            negated,
        } => {
            // LIKE/GLOB only when the pattern is a compile-time literal or
            // a positional parameter and there's no ESCAPE clause.
            if escape.is_some() {
                return None;
            }
            let glob = match op {
                LikeOp::Like => false,
                LikeOp::Glob => true,
                // No regex engine: fall back to LIKE semantics, but keep
                // the general path so the fallback stays in one place.
                LikeOp::Regexp | LikeOp::Match => return None,
            };
            let Expr::Column { table: ref_t, name } = expr.as_ref() else {
                return None;
            };
            let matches = ref_t
                .as_ref()
                .map(|t| {
                    // SQL scoping: an alias REPLACES the table name —
                    // `t.col` must NOT bind to a `FROM t t2` instance
                    // (otherwise a correlated reference to an outer
                    // un-aliased `t` is silently captured by the inner
                    // alias and compared against itself).
                    if prefix == table.name {
                        t == &table.name || t == prefix
                    } else {
                        t == prefix
                    }
                })
                .unwrap_or(true);
            if !matches {
                return None;
            }
            let col = table.find_column(name)?;
            let pat = bind_leaf(pattern, table, prefix)?;
            if matches!(pat, PredValue::Col(_)) {
                return None; // column-vs-column LIKE: general path
            }
            // `%needle%` fast path: LIKE (not GLOB), no negation, TEXT
            // literal pattern of the exact shape `%<ascii-needle>%` with
            // no other wildcards. Pre-classified once at plan time; the
            // per-row work is a single byte-substring search.
            if !glob && !negated {
                if let PredValue::Literal(Value::Text(t)) = &pat {
                    if let Some(needle) = classify_contains(t.as_bytes()) {
                        return Some(CompiledPredicate::LikeSubstr { col, needle });
                    }
                }
            }
            Some(CompiledPredicate::Like {
                col,
                pattern: pat,
                negated: *negated,
                glob,
            })
        }
        _ => None,
    }
}

/// Classify a LIKE pattern as a plain `%needle%` contains search.
/// Returns the ASCII-LOWERED needle bytes when the pattern is exactly
/// `%` + ASCII needle + `%` with no other `%`/`_` inside and a non-empty
/// needle. Lowering once at plan time means the per-row search folds
/// only the haystack bytes.
fn classify_contains(p: &[u8]) -> Option<Vec<u8>> {
    if p.len() < 3 || p[0] != b'%' || p[p.len() - 1] != b'%' {
        return None;
    }
    let needle = &p[1..p.len() - 1];
    if needle.is_empty() || !needle.is_ascii() {
        return None;
    }
    if needle.iter().any(|&b| b == b'%' || b == b'_') {
        return None;
    }
    Some(needle.to_ascii_lowercase())
}

/// Column indices referenced by a compiled expression (for building the
/// selective-decode column list).
pub(crate) fn compiled_expr_columns(e: &CompiledExpr, out: &mut Vec<usize>) {
    match e {
        CompiledExpr::Col(i) => out.push(*i),
        CompiledExpr::Param(_) | CompiledExpr::Literal(_) => {}
        CompiledExpr::Unary(_, a) => compiled_expr_columns(a, out),
        CompiledExpr::Binary(_, l, r) => {
            compiled_expr_columns(l, out);
            compiled_expr_columns(r, out);
        }
    }
}

/// Compile an expression against a table (scoped by alias `prefix`),
/// accepting BOTH bare `col` references and `prefix.col` / `table.col`
/// qualified references (the general `compile_expr` only accepts bare
/// ones). Returns None for anything outside the supported shape or any
/// reference that doesn't bind to this table (correlated/outer refs fall
/// back to the general AST-walk path).
pub(crate) fn compile_expr_scoped(
    e: &Expr,
    table: &crate::schema::Table,
    prefix: &str,
    params_len: usize,
) -> Option<CompiledExpr> {
    let col_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let normalized = normalize_scope(e, table, prefix)?;
    compile_expr(&normalized, &col_names, params_len)
}

/// Rewrite an expression tree so in-scope qualified column references
/// become bare references (compile_expr's input shape). Returns None if
/// any reference is out of scope (can't compile against this table).
fn normalize_scope(e: &Expr, table: &crate::schema::Table, prefix: &str) -> Option<Expr> {
    match e {
        Expr::Column { table: ref_t, name } => match ref_t {
            None => Some(e.clone()),
            Some(t) => {
                let matches = if prefix == table.name {
                    t == &table.name || t == prefix
                } else {
                    t == prefix
                };
                if matches && table.find_column(name).is_some() {
                    Some(Expr::Column {
                        table: None,
                        name: name.clone(),
                    })
                } else {
                    None
                }
            }
        },
        Expr::Literal(_) => Some(e.clone()),
        Expr::Parameter(_) => Some(e.clone()),
        Expr::Unary { op, expr } => Some(Expr::Unary {
            op: *op,
            expr: Box::new(normalize_scope(expr, table, prefix)?),
        }),
        Expr::Binary { op, left, right } => {
            // AND/OR are predicates, not value expressions — exclude here
            // (compile_expr would reject them anyway).
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                return None;
            }
            Some(Expr::Binary {
                op: *op,
                left: Box::new(normalize_scope(left, table, prefix)?),
                right: Box::new(normalize_scope(right, table, prefix)?),
            })
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
        if let crate::sql::ast::Statement::Create(crate::sql::ast::CreateStatement::Table {
            columns,
            constraints,
            ..
        }) = stmt
        {
            crate::schema::build_table(
                "t",
                &columns,
                &constraints,
                1,
                false,
                false,
                "CREATE TABLE t",
            )
            .unwrap()
        } else {
            panic!("not a create table")
        }
    }

    #[test]
    fn compile_simple_cmp() {
        let t = table();
        let e = Expr::Binary {
            op: BinaryOp::Gt,
            left: Box::new(Expr::Column {
                table: None,
                name: "val".into(),
            }),
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
                left: Box::new(Expr::Column {
                    table: None,
                    name: "val".into(),
                }),
                right: Box::new(Expr::Parameter("0".into())),
            }),
            right: Box::new(Expr::Binary {
                op: BinaryOp::Lt,
                left: Box::new(Expr::Column {
                    table: None,
                    name: "score".into(),
                }),
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
            expr: Box::new(Expr::Column {
                table: None,
                name: "val".into(),
            }),
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

        let n = Expr::IsNull {
            expr: Box::new(Expr::Column {
                table: None,
                name: "name".into(),
            }),
            negated: false,
        };
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

// ============================================================================
// Compiled assignment expressions
// ============================================================================
//
// UPDATE's `SET col = expr` re-evaluates `expr` per row through the general
// AST walk + name resolution (~80-120 ns/row). For the common shapes —
// column refs, literals, params, and arithmetic over them — the expression
// compiles ONCE per statement into a positional tree that evaluates in
// ~5-15 ns with no name lookups.

use crate::sql::ast::UnaryOp;

/// A compiled `SET` expression.
#[derive(Clone, Debug)]
pub(crate) enum CompiledExpr {
    Col(usize),
    Param(usize),
    Literal(Value),
    Unary(UnaryOp, Box<CompiledExpr>),
    Binary(BinaryOp, Box<CompiledExpr>, Box<CompiledExpr>),
}

impl CompiledExpr {
    #[inline]
    pub fn eval(&self, row: &[Value], params: &[Value]) -> Value {
        match self {
            CompiledExpr::Col(i) => row.get(*i).cloned().unwrap_or(Value::Null),
            CompiledExpr::Param(i) => params.get(*i).cloned().unwrap_or(Value::Null),
            CompiledExpr::Literal(v) => v.clone(),
            CompiledExpr::Unary(op, e) => {
                let v = e.eval(row, params);
                crate::executor::expr::apply_unary(*op, &v)
            }
            CompiledExpr::Binary(op, l, r) => {
                let lv = l.eval(row, params);
                let rv = r.eval(row, params);
                apply_binary(*op, &lv, &rv)
            }
        }
    }
}

/// Compile an assignment expression against a table's column list.
/// Returns None for shapes outside the supported set (caller falls back
/// to the general AST-walk path).
pub(crate) fn compile_expr(
    e: &Expr,
    col_names: &[String],
    params_len: usize,
) -> Option<CompiledExpr> {
    match e {
        Expr::Literal(v) => Some(CompiledExpr::Literal(v.clone())),
        Expr::Column { table: None, name } => {
            let idx = col_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))?;
            Some(CompiledExpr::Col(idx))
        }
        Expr::Column { table: Some(_), .. } => None, // qualified refs resolve via the general path
        Expr::Unary { op, expr } => {
            let inner = compile_expr(expr, col_names, params_len)?;
            Some(CompiledExpr::Unary(*op, Box::new(inner)))
        }
        Expr::Binary { op, left, right } => {
            // Arithmetic/comparison only; AND/OR short-circuit semantics
            // differ from eager evaluation — keep those on the general path.
            if matches!(op, BinaryOp::And | BinaryOp::Or) {
                return None;
            }
            let l = compile_expr(left, col_names, params_len)?;
            let r = compile_expr(right, col_names, params_len)?;
            Some(CompiledExpr::Binary(*op, Box::new(l), Box::new(r)))
        }
        // ? positional parameters arrive as Parameter(name); numeric
        // names index params directly (bare "?" takes the next slot —
        // the executor binds those sequentially, so treat it as 0 only
        // when it is the sole placeholder).
        Expr::Parameter(name) => {
            if name == "?" || name.is_empty() {
                return Some(CompiledExpr::Param(0));
            }
            if let Ok(idx1) = name.parse::<usize>() {
                let idx = if idx1 == 0 { 0 } else { idx1 - 1 };
                if idx < params_len || params_len == 0 {
                    return Some(CompiledExpr::Param(idx));
                }
            }
            None
        }
        _ => None,
    }
}
