//! Expression evaluator.
//!
//! Evaluates an `Expr` against a row, given a schema (list of column names
//! and a function that maps column refs to values). Built for clarity over
//! speed; a production engine would JIT-compile hot expressions.

/// SQLite version we report for `SELECT sqlite_version()` / `sqlite3_libversion()`.
/// Aligned with the C ABI compatibility layer (see compat/). ORMs key feature
/// detection off this value (e.g. RETURNING requires >= 3.35).
pub const SQLITE_COMPAT_VERSION: &str = "3.50.4";

use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::types::{Affinity, Value};
use std::collections::HashMap;

/// Resolve a `substr(X, Y, Z)` range to 0-based `[begin, end)` bounds
/// over a value of `n` units (characters for TEXT, bytes for BLOB),
/// implementing SQLite's exact algorithm from `substrFunc` (func.c):
///
///   - `Y > 0` is a 1-based start; `Y < 0` counts from the end; `Y == 0`
///     consumes one unit of the length budget and starts at 0.
///   - `Z > 0` is a length; `Z < 0` selects `|Z|` units PRECEDING the
///     start; omitted `Z` means "to the end".
///   - A start left of the beginning also eats into the length budget.
fn substr_range(n: i64, y: i64, z: i64) -> (i64, i64) {
    let mut p1 = y;
    let mut p2 = z;
    if p1 < 0 {
        p1 = p1.saturating_add(n);
        if p1 < 0 {
            p2 = p2.saturating_add(p1);
            if p2 < 0 {
                p2 = 0;
            }
            p1 = 0;
        }
    } else if p1 > 0 {
        p1 -= 1;
    } else if p2 > 0 {
        // Position 0 does not exist: the missing first character still
        // consumes one unit of length (substr('hello', 0, 2) = 'h').
        p2 -= 1;
    }
    if p2 < 0 {
        // |Z| units preceding the start position, clamped to the string.
        let begin = (p1 + p2).max(0);
        let end = p1.min(n);
        if begin >= end {
            (0, 0)
        } else {
            (begin, end)
        }
    } else {
        let begin = p1.min(n).max(0);
        let end = p1.saturating_add(p2).min(n).max(begin);
        (begin, end)
    }
}

/// CAST(value AS type) with SQLite's exact semantics — which differ from
/// column-affinity coercion: CAST parses the longest numeric PREFIX of the
/// text (`CAST('12abc' AS INTEGER)` is 12, `CAST('abc' AS INTEGER)` is 0),
/// while affinity conversion only fires when the WHOLE string looks numeric.
/// Overflow saturates to i64::MIN/MAX, `CAST('inf' AS REAL)` is 0.0, and
/// NUMERIC keeps integral values as INTEGER (`CAST('12.0' AS NUMERIC)` is
/// the integer 12).
fn cast_value(v: Value, type_name: &str) -> Value {
    let affinity = Affinity::from_declared_type(type_name);
    match affinity {
        Affinity::Integer => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => Value::Integer(i),
            // Rust's float→int `as` saturates exactly like SQLite clamps.
            Value::Real(f) => Value::Integer(f as i64),
            Value::Text(_) | Value::Blob(_) => Value::Integer(parse_int_prefix(&v.as_text())),
        },
        Affinity::Real => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => Value::Real(i as f64),
            Value::Real(f) => Value::Real(f),
            Value::Text(_) | Value::Blob(_) => Value::Real(parse_real_prefix(&v.as_text())),
        },
        Affinity::Text => match v {
            Value::Null => Value::Null,
            other => Value::Text(other.as_text().into()),
        },
        Affinity::Blob => match v {
            Value::Null => Value::Null,
            Value::Blob(b) => Value::Blob(b),
            other => Value::Blob(other.as_text().into_bytes()),
        },
        // No declared type (CAST(x AS) is a syntax error anyway): no-op.
        Affinity::None => v,
    }
}

/// SQLite `sqlite3Atoi64` prefix semantics for CAST(... AS INTEGER):
/// optional whitespace and sign, then digits. No digits -> 0. No exponent
/// (`CAST('1e3' AS INTEGER)` is 1). Overflow saturates to i64::MIN/MAX.
fn parse_int_prefix(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut v: i64 = 0;
    let mut digits = 0usize;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        let d = (b[i] - b'0') as i64;
        match v.checked_mul(10).and_then(|x| x.checked_add(d)) {
            Some(nv) => v = nv,
            None => overflow = true,
        }
        digits += 1;
        i += 1;
    }
    if digits == 0 {
        return 0;
    }
    if overflow {
        return if neg { i64::MIN } else { i64::MAX };
    }
    if neg {
        -v
    } else {
        v
    }
}

/// SQLite `sqlite3AtoF` prefix semantics for CAST(... AS REAL):
/// optional whitespace and sign, digits with optional fraction, and an
/// optional exponent that only counts when at least one digit follows.
/// `inf`/`nan` are NOT accepted by CAST (they yield 0.0).
fn parse_real_prefix(s: &str) -> f64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let mut mantissa_digits = 0usize;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        mantissa_digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            mantissa_digits += 1;
        }
    }
    if mantissa_digits == 0 {
        return 0.0;
    }
    let mut end = i;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let mut exp_digits = 0usize;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
            exp_digits += 1;
        }
        if exp_digits > 0 {
            end = j;
        }
    }
    s[start..end].parse::<f64>().unwrap_or(0.0)
}

/// SQLite's `round(X, N)`: round the EXACT binary value of X to N digits,
/// ties away from zero. The naive `x * 10^N` scale-round-divide loses to
/// representation error (2.675 * 100 rounds to exactly 267.5 in f64, so
/// it would round to 2.68 — SQLite gives 2.67 because 2.675 is actually
/// 2.67499999... in binary). The FMA error term recovers the direction of
/// the true product when the scaled value lands exactly on a .5 boundary.
fn sqlite_round(x: f64, n: i64) -> f64 {
    if x.is_infinite() {
        return x;
    }
    let n = n.clamp(0, 30);
    // |x| >= 2^52 is already an exact integer; rounding to N >= 0 digits
    // cannot change it (SQLite short-circuits the same range).
    if x.abs() >= 4_503_599_627_370_496.0 {
        return x;
    }
    if n == 0 {
        // SQLite: (i64)(r + (r < 0 ? -0.5 : +0.5)) — half away from zero.
        return (x + if x < 0.0 { -0.5 } else { 0.5 }).trunc();
    }
    let factor = 10f64.powi(n as i32);
    let scaled = x * factor;
    if !scaled.is_finite() {
        return x;
    }
    // Exact rounding error of the multiplication (FMA is correctly
    // rounded): true_product == scaled + err exactly.
    let err = f64::mul_add(x, factor, -scaled);
    let rounded = if scaled.fract().abs() == 0.5 && err != 0.0 {
        // The scaled value sits exactly on a half boundary but the TRUE
        // product is on one side of it: round toward the true side.
        // err > 0 means the true product is greater than `scaled`.
        let true_is_above = err > 0.0;
        let scaled_is_positive = scaled > 0.0;
        if true_is_above == scaled_is_positive {
            // True value lies AWAY from zero relative to the boundary.
            scaled.round()
        } else {
            scaled.trunc()
        }
    } else {
        // Either not at a boundary (plain round is exact) or an exact
        // mathematical tie (SQLite rounds half away from zero).
        scaled.round()
    };
    rounded / factor
}

/// A row context: maps column references (table, name) to values.
pub struct EvalContext<'a> {
    /// Per-table column values, indexed by table alias.
    /// The key is the alias (or table name if no alias).
    pub tables: HashMap<String, &'a [Value]>,
    /// Anonymous row: used when there's exactly one source and column refs
    /// don't qualify the table.
    pub row: &'a [Value],
    /// Column names for the anonymous row (used for unqualified refs).
    pub column_names: &'a [String],
    /// Bound positional parameters (? placeholder), indexed 0..N.
    /// This is the **common case** — virtually all real-world queries use
    /// anonymous `?` placeholders, so the hot path is a single Vec index.
    /// Previously this was a `HashMap<String, Value>` which allocated a
    /// bucket array on first insert (~200-500 ns per query) and required
    /// a hash + lookup per evaluation. The Vec is pre-sized by the
    /// caller (ExecContext) and indexed by usize.
    pub params: &'a [Value],
    /// Named parameters (:name, @col, $var). Allocated lazily — empty for
    /// the 99% case of purely positional `?` placeholders.
    pub named_params: &'a HashMap<String, Value>,
}

impl<'a> EvalContext<'a> {
    pub fn new(
        row: &'a [Value],
        column_names: &'a [String],
        params: &'a [Value],
        named_params: &'a HashMap<String, Value>,
    ) -> Self {
        Self {
            tables: HashMap::new(),
            row,
            column_names,
            params,
            named_params,
        }
    }

    pub fn add_table(&mut self, alias: &str, row: &'a [Value]) {
        self.tables.insert(alias.to_ascii_lowercase(), row);
    }

    /// Look up a column reference. Returns the value or NULL if not found.
    pub fn lookup(&self, table: &Option<String>, name: &str) -> Value {
        if let Some(t) = table {
            // Try qualified lookups: "alias.column" or "table.column".
            let qual_lower = format!("{}.{}", t.to_ascii_lowercase(), name.to_ascii_lowercase());
            for (i, n) in self.column_names.iter().enumerate() {
                if n.to_ascii_lowercase() == qual_lower {
                    return self.row.get(i).cloned().unwrap_or(Value::Null);
                }
            }
            // Qualified ref that doesn't match a local qualified name: if an
            // outer scope knows "qual.column", it is a correlated reference
            // (SQL scope rules — the qualifier names an outer table). This
            // MUST be consulted BEFORE the local unqualified fallback, or an
            // inner column with the same bare name would shadow the outer
            // reference (`u2.active = u.active` would compare u2 to u2).
            if let Some(v) = crate::executor::corr_outer_qualified(t, name) {
                return v;
            }
            // Fall back: try unqualified (the first column with this name).
            return self.lookup_in_main(name);
        }
        self.lookup_in_main(name)
    }

    fn lookup_in_main(&self, name: &str) -> Value {
        // Special column: rowid / _rowid_ / oid
        if name.eq_ignore_ascii_case("rowid")
            || name.eq_ignore_ascii_case("_rowid_")
            || name.eq_ignore_ascii_case("oid")
        {
            if let Some(idx) = self
                .column_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case("rowid"))
            {
                return self.row.get(idx).cloned().unwrap_or(Value::Null);
            }
        }
        // Try exact match first.
        for (i, n) in self.column_names.iter().enumerate() {
            if n.eq_ignore_ascii_case(name) {
                return self.row.get(i).cloned().unwrap_or(Value::Null);
            }
        }
        // Try qualified match by suffix (e.g. "u.id" matches "id").
        for (i, n) in self.column_names.iter().enumerate() {
            if let Some(pos) = n.rfind('.') {
                let suffix = &n[pos + 1..];
                if suffix.eq_ignore_ascii_case(name) {
                    return self.row.get(i).cloned().unwrap_or(Value::Null);
                }
            }
        }
        // Not found locally — correlated-subquery outer scope (innermost
        // frame first). No-op when no correlated subquery is executing.
        if let Some(v) = crate::executor::corr_outer_lookup(None, name) {
            return v;
        }
        Value::Null
    }
}

/// Evaluate an expression in the given context.
pub fn evaluate(expr: &Expr, ctx: &EvalContext<'_>) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Parameter(p) => {
            // Fast path: numeric parameter name → positional index.
            // `?` placeholders lex to "0", "1", "2", ... so the common path
            // is a single Vec index. Named params (:name, @col, $var) fall
            // through to the HashMap.
            if let Ok(idx) = p.parse::<usize>() {
                Ok(ctx.params.get(idx).cloned().unwrap_or(Value::Null))
            } else {
                Ok(ctx.named_params.get(p).cloned().unwrap_or(Value::Null))
            }
        }
        Expr::Column { table, name } => Ok(ctx.lookup(table, name)),
        Expr::Binary { op, left, right } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            // COLLATE on either comparison operand applies a collation to
            // text comparison (SQLite: `a < b COLLATE NOCASE`). Only
            // ordering / equality operators honor it.
            if let Some(coll_name) =
                comparison_collation(left).or_else(|| comparison_collation(right))
            {
                if let Some(coll) = crate::plugin::lookup_collation(&coll_name) {
                    return Ok(apply_binary_collated(*op, &l, &r, coll.as_ref()));
                }
                return Err(crate::error::Error::semantic(format!(
                    "no such collation sequence: {}",
                    coll_name
                )));
            }
            Ok(apply_binary(*op, &l, &r))
        }
        Expr::Unary { op, expr } => {
            let v = evaluate(expr, ctx)?;
            Ok(apply_unary(*op, &v))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = evaluate(expr, ctx)?;
            let lo = evaluate(low, ctx)?;
            let hi = evaluate(high, ctx)?;
            // SQL three-valued logic: BETWEEN is sugar for `expr >= low AND
            // expr <= high`. If any operand is NULL, the AND result is NULL
            // (UNKNOWN), which WHERE filters out. So a NULL val yields NULL
            // — meaning `val BETWEEN 20 AND 40` returns no row for NULL val,
            // and `val NOT BETWEEN 20 AND 40` ALSO returns no row (because
            // NOT NULL is still NULL).
            if v.is_null() || lo.is_null() || hi.is_null() {
                return Ok(Value::Null);
            }
            // A COLLATE on the value operand applies to both bound
            // comparisons (SQLite).
            let in_range = if let Some(name) = comparison_collation(expr) {
                let coll = crate::plugin::lookup_collation(&name).ok_or_else(|| {
                    crate::error::Error::semantic(format!("no such collation sequence: {}", name))
                })?;
                let ge = crate::plugin::compare_collated(&v, &lo, coll.as_ref())
                    != std::cmp::Ordering::Less;
                let le = crate::plugin::compare_collated(&v, &hi, coll.as_ref())
                    != std::cmp::Ordering::Greater;
                ge && le
            } else {
                v >= lo && v <= hi
            };
            Ok(Value::Integer(if in_range ^ negated { 1 } else { 0 }))
        }
        Expr::In {
            expr,
            source,
            negated,
        } => evaluate_in(expr, source, *negated, ctx),
        Expr::Like {
            op,
            expr,
            pattern,
            escape,
            negated,
        } => {
            let v = evaluate(expr, ctx)?;
            let p = evaluate(pattern, ctx)?;
            let esc = if let Some(e) = escape {
                Some(evaluate(e, ctx)?)
            } else {
                None
            };
            // Three-valued logic: any NULL operand makes the whole
            // comparison NULL (unknown) — NOT LIKE included — so WHERE
            // filters the row out either way (SQLite semantics).
            if v.is_null() || p.is_null() || esc.as_ref().map(|e| e.is_null()).unwrap_or(false) {
                return Ok(Value::Null);
            }
            let result = match op {
                LikeOp::Like => like_match(&v, &p, esc.as_ref(), false),
                LikeOp::Glob => glob_match(&v, &p),
                LikeOp::Regexp | LikeOp::Match => {
                    // We don't ship a regex engine; fall back to LIKE semantics.
                    like_match(&v, &p, esc.as_ref(), false)
                }
            };
            Ok(Value::Integer(if result ^ negated { 1 } else { 0 }))
        }
        Expr::IsNull { expr, negated } => {
            let v = evaluate(expr, ctx)?;
            let is_null = v.is_null();
            Ok(Value::Integer(if is_null ^ negated { 1 } else { 0 }))
        }
        Expr::Is {
            left,
            right,
            negated,
        } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            // IS treats NULL as equal to NULL.
            let equal = if l.is_null() && r.is_null() {
                true
            } else if l.is_null() || r.is_null() {
                false
            } else {
                l == r
            };
            Ok(Value::Integer(if equal ^ negated { 1 } else { 0 }))
        }
        Expr::Function { name, args, .. } => evaluate_function(name, args, ctx),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            let op_val = if let Some(o) = operand {
                Some(evaluate(o, ctx)?)
            } else {
                None
            };
            for (cond, val) in whens {
                if let Some(op) = &op_val {
                    let c = evaluate(cond, ctx)?;
                    if op == &c {
                        return evaluate(val, ctx);
                    }
                } else {
                    let c = evaluate(cond, ctx)?;
                    if c.is_truthy() {
                        return evaluate(val, ctx);
                    }
                }
            }
            if let Some(e) = else_ {
                Ok(evaluate(e, ctx)?)
            } else {
                Ok(Value::Null)
            }
        }
        Expr::Row(_) => Err(Error::Unsupported("row value expressions in this context")),
        Expr::Subquery(sel) => {
            // Correlated subqueries execute per-row through the statement
            // bridge (see executor::corr). Uncorrelated ones were already
            // substituted at plan-rewrite time; reaching this arm means the
            // bridge must be active — e.g. a correlated ref inside a DML
            // SET/CHECK expression evaluated outside a plan rewrite.
            crate::executor::corr_exec_scalar(sel, ctx)
        }
        Expr::Exists(sel) => crate::executor::corr_exec_exists(sel, ctx),
        Expr::Cast { expr, type_name } => {
            let v = evaluate(expr, ctx)?;
            Ok(cast_value(v, type_name))
        }
        // COLLATE: transparent for evaluation (the collation is consumed
        // by comparison operators and ORDER BY — see the Binary arm and
        // exec_sort).
        Expr::Collate { expr, .. } => evaluate(expr, ctx),
        Expr::Raise { action, .. } => Err(Error::runtime(format!("RAISE {:?}", action))),
    }
}

fn evaluate_in(
    expr: &Expr,
    source: &InSource,
    negated: bool,
    ctx: &EvalContext<'_>,
) -> Result<Value> {
    let v = evaluate(expr, ctx)?;
    // A COLLATE on the left operand applies to every membership
    // comparison (SQLite: `v COLLATE NOCASE IN ('a', 'B')`).
    let coll = comparison_collation(expr).and_then(|name| {
        crate::plugin::lookup_collation(&name).or_else(|| {
            // Unknown collation name: error later at the comparison below
            // would abort the whole scan — mirror the binary comparison
            // behavior and error here instead.
            None
        })
    });
    if let (Some(name), None) = (comparison_collation(expr), &coll) {
        return Err(crate::error::Error::semantic(format!(
            "no such collation sequence: {}",
            name
        )));
    }
    // SQL three-valued logic for IN:
    //   - If v is NULL: result is NULL (regardless of list contents).
    //   - If v matches a non-NULL list element: result is TRUE (FALSE if negated).
    //   - If v doesn't match any list element AND the list contains NULL:
    //     result is NULL (because we can't rule out that v == NULL).
    //   - If v doesn't match any list element AND the list has no NULL:
    //     result is FALSE (TRUE if negated).
    // Previously we treated Null == Null as true via our PartialEq, which
    // made `WHERE v IN (NULL)` match every NULL row — caught by the
    // differential test 'null_in_list_with_null_returns_null_only_for_null_row'.
    if v.is_null() {
        return Ok(Value::Null);
    }
    let coll_ref = coll.as_deref();
    let (found, list_has_null) = match source {
        InSource::List(list) => {
            let mut found = false;
            let mut list_has_null = false;
            for e in list {
                let candidate = evaluate(e, ctx)?;
                if candidate.is_null() {
                    list_has_null = true;
                    continue;
                }
                let eq = match coll_ref {
                    Some(c) => {
                        crate::plugin::compare_collated(&v, &candidate, c)
                            == std::cmp::Ordering::Equal
                    }
                    None => v == candidate,
                };
                if eq {
                    found = true;
                    // Keep iterating to detect NULLs (we need list_has_null
                    // accurate even after a match, for the negated case).
                }
            }
            (found, list_has_null)
        }
        InSource::Subquery(sel) => {
            // Correlated IN-subquery: execute per-row through the bridge
            // (uncorrelated ones became literal lists at rewrite time).
            let list = crate::executor::corr_exec_in_list(sel, ctx)?;
            let mut found = false;
            let mut list_has_null = false;
            for candidate in list {
                if candidate.is_null() {
                    list_has_null = true;
                    continue;
                }
                if v == candidate {
                    found = true;
                }
            }
            (found, list_has_null)
        }
        InSource::Table(_) => {
            return Err(Error::Unsupported("IN table via evaluator (use executor)"));
        }
    };
    if found {
        // v matches a list element → result is TRUE / FALSE (NOT IN).
        Ok(Value::Integer(if negated { 0 } else { 1 }))
    } else if list_has_null {
        // v didn't match any non-NULL element, but list has a NULL —
        // we can't rule out a match against NULL, so the result is NULL.
        Ok(Value::Null)
    } else {
        // v definitely doesn't match any element → FALSE / TRUE (NOT IN).
        Ok(Value::Integer(if negated { 1 } else { 0 }))
    }
}

fn evaluate_function(name: &str, args: &[Expr], ctx: &EvalContext<'_>) -> Result<Value> {
    let fname = name.to_ascii_lowercase();
    // Scalar functions only here; aggregates are handled by the Aggregate operator.
    let argvals: Result<Vec<Value>> = args.iter().map(|e| evaluate(e, ctx)).collect();
    let argvals = argvals?;
    call_scalar(&fname, &argvals)
}

/// Call a scalar SQL function.
pub fn call_scalar(name: &str, args: &[Value]) -> Result<Value> {
    let fname = name.to_ascii_lowercase();
    Ok(match fname.as_str() {
        "abs" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            // SQLite: abs() of the most-negative integer raises
            // "integer overflow" (the value has no positive i64).
            Some(Value::Integer(i)) if *i == i64::MIN => {
                return Err(Error::runtime("integer overflow"));
            }
            Some(Value::Integer(i)) => Value::Integer(i.abs()),
            Some(Value::Real(f)) => Value::Real(f.abs()),
            // Numeric-looking text gets coerced; everything else: SQLite returns 0.
            Some(other) => {
                let i = other.as_integer();
                if i == i64::MIN {
                    return Err(Error::runtime("integer overflow"));
                }
                Value::Integer(i.abs())
            }
        },
        "length" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Integer(v.length()),
        },
        "lower" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_lowercase().into()),
        },
        "upper" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_uppercase().into()),
        },
        // TRIM(x) / TRIM(x, chars) — strip (chars, default whitespace)
        // from both ends / left / right. The char set is UTF-8 code points.
        "trim" => match (args.first(), args.get(1)) {
            (Some(Value::Null) | None, _) => Value::Null,
            (Some(v), None) => Value::Text(v.as_text().trim().to_string().into()),
            (Some(v), Some(cs)) if !cs.is_null() => {
                let set: Vec<char> = cs.as_text().chars().collect();
                let s: String = v.as_text().chars().collect();
                Value::Text(s.trim_matches(|c| set.contains(&c)).to_string().into())
            }
            (Some(v), Some(_)) => Value::Text(v.as_text().trim().to_string().into()),
        },
        "ltrim" => match (args.first(), args.get(1)) {
            (Some(Value::Null) | None, _) => Value::Null,
            (Some(v), None) => Value::Text(v.as_text().trim_start().to_string().into()),
            (Some(v), Some(cs)) if !cs.is_null() => {
                let set: Vec<char> = cs.as_text().chars().collect();
                let s: String = v.as_text().chars().collect();
                Value::Text(
                    s.trim_start_matches(|c| set.contains(&c))
                        .to_string()
                        .into(),
                )
            }
            (Some(v), Some(_)) => Value::Text(v.as_text().trim_start().to_string().into()),
        },
        "rtrim" => match (args.first(), args.get(1)) {
            (Some(Value::Null) | None, _) => Value::Null,
            (Some(v), None) => Value::Text(v.as_text().trim_end().to_string().into()),
            (Some(v), Some(cs)) if !cs.is_null() => {
                let set: Vec<char> = cs.as_text().chars().collect();
                let s: String = v.as_text().chars().collect();
                Value::Text(s.trim_end_matches(|c| set.contains(&c)).to_string().into())
            }
            (Some(v), Some(_)) => Value::Text(v.as_text().trim_end().to_string().into()),
        },
        // LIKELY(x) / UNLIKELY(x) — query-planner no-ops, identity.
        "likely" | "unlikely" => match args.first() {
            Some(v) => v.clone(),
            None => Value::Null,
        },
        "replace" => {
            if args.len() == 3 && args.iter().all(|v| !v.is_null()) {
                let s = args[0].as_text();
                let from = args[1].as_text();
                let to = args[2].as_text();
                // Empty needle: SQLite returns the string unchanged
                // (str::replace would insert `to` between every char).
                if from.is_empty() {
                    Value::Text(s.into())
                } else {
                    Value::Text(s.replace(&from, &to).into())
                }
            } else {
                Value::Null
            }
        }
        "substr" | "substring" => {
            if args.len() >= 2
                && args.iter().take(2).all(|v| !v.is_null())
                && (args.len() < 3 || !args[2].is_null())
            {
                // Blob operands are byte-indexed and yield a blob
                // (mirrors SQLite: substr(x'00ff', 2, 1) = x'ff').
                if let Some(Value::Blob(b)) = args.first() {
                    let (begin, end) = substr_range(
                        b.len() as i64,
                        args[1].as_integer(),
                        if args.len() == 3 {
                            args[2].as_integer()
                        } else {
                            i64::MAX / 4
                        },
                    );
                    return Ok(Value::Blob(b[begin as usize..end as usize].to_vec()));
                }
                let s = args[0].as_text();
                let z = if args.len() == 3 {
                    args[2].as_integer()
                } else {
                    i64::MAX / 4
                };
                if s.is_ascii() {
                    // Fast path: byte index == char index for ASCII.
                    let (begin, end) = substr_range(s.len() as i64, args[1].as_integer(), z);
                    Value::Text(s[begin as usize..end as usize].into())
                } else {
                    // Count UTF-8 characters, never slice mid-codepoint.
                    let n = s.chars().count() as i64;
                    let (begin, end) = substr_range(n, args[1].as_integer(), z);
                    let out: String = s
                        .chars()
                        .skip(begin as usize)
                        .take((end - begin) as usize)
                        .collect();
                    Value::Text(out.into())
                }
            } else {
                Value::Null
            }
        }
        "coalesce" | "ifnull" => {
            for v in args {
                if !v.is_null() {
                    return Ok(v.clone());
                }
            }
            Value::Null
        }
        "nullif" => {
            if args.len() == 2 {
                if args[0] == args[1] {
                    Value::Null
                } else {
                    args[0].clone()
                }
            } else {
                Value::Null
            }
        }
        "iif" => {
            if args.len() == 3 {
                if args[0].is_truthy() {
                    args[1].clone()
                } else {
                    args[2].clone()
                }
            } else {
                Value::Null
            }
        }
        "round" => {
            if args.is_empty() || args[0].is_null() {
                Value::Null
            } else {
                let x = args[0].as_real();
                if x.is_nan() {
                    // SQLite: round(NaN) is NULL.
                    return Ok(Value::Null);
                }
                let n = args.get(1).map(|v| v.as_integer()).unwrap_or(0);
                Value::Real(sqlite_round(x, n))
            }
        }
        "random" => Value::Integer(rand_i64()),
        "randomblob" => {
            let n = args.first().map(|v| v.as_integer()).unwrap_or(0).max(0) as usize;
            Value::Blob(
                (0..n)
                    .map(|i| (i.wrapping_mul(2654435761) & 0xFF) as u8)
                    .collect(),
            )
        }
        // __JSON_OBJECT_FRAG(k, v) — hidden scalar feeding
        // json_group_object's accumulator: `"k":v` (JSON-quoted).
        "__json_object_frag" => {
            if args.len() == 2 && !args[0].is_null() && !args[1].is_null() {
                let k = crate::executor::json::json_quote_value(&args[0]);
                let v = crate::executor::json::json_quote_value(&args[1]);
                Value::Text(format!("{k}:{v}").into())
            } else {
                Value::Null
            }
        }
        // UNHEX(x) — hex string -> blob; NULL on invalid hex or odd length.
        "unhex" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                if s.len() % 2 != 0 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Value::Null
                } else {
                    let mut out = Vec::with_capacity(s.len() / 2);
                    let bytes = s.as_bytes();
                    for pair in bytes.chunks(2) {
                        let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
                        let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
                        out.push((hi << 4) | lo);
                    }
                    Value::Blob(out)
                }
            }
        },
        "hex" => {
            // BLOB input hexes the RAW bytes (as_text lossy-converts
            // invalid UTF-8, which corrupted 0xff into U+FFFD).
            let out = match args.first() {
                Some(Value::Blob(b)) => b.iter().map(|x| format!("{:02X}", x)).collect::<String>(),
                Some(v) => v
                    .as_text()
                    .bytes()
                    .map(|b| format!("{:02X}", b))
                    .collect::<String>(),
                None => String::new(),
            };
            Value::Text(out.into())
        }
        "typeof" => Value::Text(
            match args.first() {
                Some(Value::Null) => "null",
                Some(Value::Integer(_)) => "integer",
                Some(Value::Real(_)) => "real",
                Some(Value::Text(_)) => "text",
                Some(Value::Blob(_)) => "blob",
                None => "null",
            }
            .to_string()
            .into(),
        ),
        "date" | "time" | "datetime" | "strftime" | "julianday" | "unixepoch" | "timediff" => {
            // Full SQLite-compatible date/time engine (see datetime.rs).
            crate::executor::datetime::call_datetime_function(&fname, args)
        }
        "current_date" | "current_time" | "current_timestamp" => {
            crate::executor::datetime::call_datetime_function(&fname, args)
        }
        "last_insert_rowid" => Value::Integer(crate::executor::change_counters::conn_rowid()),
        "changes" => Value::Integer(crate::executor::change_counters::last()),
        "total_changes" => Value::Integer(crate::executor::change_counters::total()),
        "sqlite_version" => Value::Text(SQLITE_COMPAT_VERSION.into()),
        "quote" => {
            let v = args.first().cloned().unwrap_or(Value::Null);
            Value::Text(quote_value(&v).into())
        }
        // INSTR(s, sub) — returns the 1-indexed position of `sub` in `s`,
        // or 0 if not found. NULL inputs return NULL.
        "instr" => {
            if args.len() != 2 || args[0].is_null() || args[1].is_null() {
                return Ok(Value::Null);
            }
            let s = args[0].as_text();
            let sub = args[1].as_text();
            if sub.is_empty() {
                return Ok(Value::Integer(1));
            }
            match s.find(&sub) {
                Some(pos) => Value::Integer((pos + 1) as i64),
                None => Value::Integer(0),
            }
        }
        // PRINTF — minimal SQLite printf implementation. Supports %d, %s,
        // %f, %x, %c, %% substitutions. NULL format returns NULL.
        "printf" | "format" => {
            if args.is_empty() || args[0].is_null() {
                return Ok(Value::Null);
            }
            let fmt = args[0].as_text();
            let mut out = String::with_capacity(fmt.len());
            let mut chars = fmt.chars().peekable();
            let mut arg_idx = 1;
            while let Some(c) = chars.next() {
                if c != '%' {
                    out.push(c);
                    continue;
                }
                // ---- parse: % [flags] [width] [.prec] conv ----
                let mut minus = false;
                let mut plus = false;
                let mut space = false;
                let mut alt = false;
                let mut zero = false;
                loop {
                    match chars.peek() {
                        Some('-') => {
                            minus = true;
                            chars.next();
                        }
                        Some('+') => {
                            plus = true;
                            chars.next();
                        }
                        Some(' ') => {
                            space = true;
                            chars.next();
                        }
                        Some('#') => {
                            alt = true;
                            chars.next();
                        }
                        Some('0') => {
                            zero = true;
                            chars.next();
                        }
                        _ => break,
                    }
                }
                let mut width = 0usize;
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        width = width
                            .saturating_mul(10)
                            .saturating_add(d as usize - '0' as usize);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let mut prec: Option<usize> = None;
                if chars.peek() == Some(&'.') {
                    chars.next();
                    let mut p = 0usize;
                    while let Some(&d) = chars.peek() {
                        if d.is_ascii_digit() {
                            p = p
                                .saturating_mul(10)
                                .saturating_add(d as usize - '0' as usize);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    prec = Some(p);
                }
                let conv = match chars.next() {
                    Some(ch) => ch,
                    None => {
                        out.push('%');
                        break;
                    }
                };
                if conv == '%' {
                    out.push('%');
                    continue;
                }
                let arg = args.get(arg_idx).cloned().unwrap_or(Value::Null);
                arg_idx += 1;
                // Raw conversion (no width yet).
                let (raw, numeric): (String, bool) = match conv {
                    'd' | 'i' => {
                        let i = arg.as_integer();
                        let mut s = i.abs().to_string();
                        if i < 0 {
                            s.insert(0, '-');
                        } else if plus {
                            s.insert(0, '+');
                        } else if space {
                            s.insert(0, ' ');
                        }
                        (s, true)
                    }
                    'u' => (format!("{}", arg.as_integer() as u64), true),
                    'x' => (format!("{:x}", arg.as_integer() as u64), true),
                    'X' => (format!("{:X}", arg.as_integer() as u64), true),
                    'o' => (format!("{:o}", arg.as_integer() as u64), true),
                    'f' | 'F' => {
                        let p = prec.unwrap_or(6);
                        (format!("{:.*}", p, arg.as_real()), true)
                    }
                    'e' => {
                        let p = prec.unwrap_or(6);
                        (format!("{:.*e}", p, arg.as_real()), true)
                    }
                    'E' => {
                        let p = prec.unwrap_or(6);
                        (format!("{:.*e}", p, arg.as_real()).to_uppercase(), true)
                    }
                    'g' | 'G' => {
                        let r = arg.as_real();
                        let s = match prec {
                            Some(p) => format!("{:.*}", p, r),
                            None => format!("{r}"),
                        };
                        (s, true)
                    }
                    's' => {
                        let s = arg.as_text();
                        let s: String = match prec {
                            Some(p) => s.chars().take(p).collect(),
                            None => s.to_string(),
                        };
                        (s, false)
                    }
                    'c' => {
                        let ch = if let Value::Integer(n) = &arg {
                            char::from_u32(*n as u32)
                        } else {
                            arg.as_text().chars().next()
                        };
                        (ch.map(|c| c.to_string()).unwrap_or_default(), false)
                    }
                    _ => {
                        // Unknown conversion: emit verbatim.
                        out.push('%');
                        out.push(conv);
                        continue;
                    }
                };
                let _ = alt;
                out.push_str(&apply_printf_width(raw, minus, zero && numeric, width));
            }
            Value::Text(out.into())
        }
        // MIN(a, b, c, ...) — scalar form (not the aggregate form).
        // Returns the smallest argument. SQLite semantics: if ANY arg is
        // NULL, the result is NULL (the comparison short-circuits).
        "min" if args.len() > 1 => {
            if args.iter().any(|v| v.is_null()) {
                return Ok(Value::Null);
            }
            let mut best: Option<Value> = None;
            for v in args {
                if best.is_none() || v < best.as_ref().unwrap() {
                    best = Some(v.clone());
                }
            }
            best.unwrap_or(Value::Null)
        }
        // MAX(a, b, c, ...) — scalar form.
        // Same NULL semantics as MIN: any NULL arg → result is NULL.
        "max" if args.len() > 1 => {
            if args.iter().any(|v| v.is_null()) {
                return Ok(Value::Null);
            }
            let mut best: Option<Value> = None;
            for v in args {
                if best.is_none() || v > best.as_ref().unwrap() {
                    best = Some(v.clone());
                }
            }
            best.unwrap_or(Value::Null)
        }
        // SIGN(x) — returns -1, 0, or +1 depending on the sign of x.
        // NULL input returns NULL.
        "sign" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let r = v.as_real();
                if r > 0.0 {
                    Value::Integer(1)
                } else if r < 0.0 {
                    Value::Integer(-1)
                } else {
                    Value::Integer(0)
                }
            }
        },
        // POWER(x, y) / POW(x, y) — x^y.
        "power" | "pow" => {
            if args.len() == 2 && !args[0].is_null() && !args[1].is_null() {
                Value::Real(args[0].as_real().powf(args[1].as_real()))
            } else {
                Value::Null
            }
        }
        // MOD(x, y) — remainder (integer flavor preserved).
        "mod" => {
            if args.len() == 2 && !args[0].is_null() && !args[1].is_null() {
                let y = args[1].as_real();
                if y == 0.0 {
                    Value::Null
                } else if let (Value::Integer(a), Value::Integer(b)) = (&args[0], &args[1]) {
                    Value::Integer(a.wrapping_rem(*b))
                } else {
                    Value::Real(args[0].as_real() % y)
                }
            } else {
                Value::Null
            }
        }
        // Trigonometry (SQLite's MATH functions extension — always on here).
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh"
        | "acosh" | "atanh" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                let r = match fname.as_str() {
                    "sin" => x.sin(),
                    "cos" => x.cos(),
                    "tan" => x.tan(),
                    "asin" => x.asin(),
                    "acos" => x.acos(),
                    "atan" => x.atan(),
                    "sinh" => x.sinh(),
                    "cosh" => x.cosh(),
                    "tanh" => x.tanh(),
                    "asinh" => x.asinh(),
                    "acosh" => x.acosh(),
                    _ => x.atanh(),
                };
                Value::Real(r)
            }
        },
        "atan2" => {
            if args.len() == 2 && !args[0].is_null() && !args[1].is_null() {
                Value::Real(args[0].as_real().atan2(args[1].as_real()))
            } else {
                Value::Null
            }
        }
        // DEGREES(x) / RADIANS(x).
        "degrees" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(v.as_real().to_degrees()),
        },
        "radians" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(v.as_real().to_radians()),
        },
        // Cotangent / secant / cosecant (SQLite math extension).
        "cot" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let t = v.as_real().tan();
                if t == 0.0 {
                    Value::Null
                } else {
                    Value::Real(1.0 / t)
                }
            }
        },
        "sec" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(1.0 / v.as_real().cos()),
        },
        "csc" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_real().sin();
                if s == 0.0 {
                    Value::Null
                } else {
                    Value::Real(1.0 / s)
                }
            }
        },
        "log2" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x <= 0.0 {
                    Value::Null
                } else {
                    Value::Real(x.log2())
                }
            }
        },
        // SQRT(x).
        "sqrt" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x < 0.0 {
                    Value::Null
                } else {
                    Value::Real(x.sqrt())
                }
            }
        },
        // FLOOR / CEIL / CEILING.
        "floor" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(v.as_real().floor()),
        },
        "ceil" | "ceiling" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(v.as_real().ceil()),
        },
        // TRUNC(x) — truncate toward zero.
        "trunc" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(v.as_real().trunc()),
        },
        // PI() — 3.141592653589793.
        "pi" => Value::Real(std::f64::consts::PI),
        // EXP(x) — e^x.
        "exp" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Real(std::f64::consts::E.powf(v.as_real())),
        },
        // LN(x) — natural log.
        "ln" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x <= 0.0 {
                    Value::Null
                } else {
                    Value::Real(x.ln())
                }
            }
        },
        // LOG(x) / LOG10(x) — base-10 log. With two args, LOG(b, x) is base-b.
        "log" | "log10" => {
            if args.is_empty() || args[0].is_null() {
                return Ok(Value::Null);
            }
            if args.len() == 2 && !args[1].is_null() {
                let b = args[0].as_real();
                let x = args[1].as_real();
                if b <= 0.0 || b == 1.0 || x <= 0.0 {
                    return Ok(Value::Null);
                }
                return Ok(Value::Real(x.log(b)));
            }
            let x = args[0].as_real();
            if x <= 0.0 {
                Value::Null
            } else {
                Value::Real(x.log10())
            }
        }
        // ABS already defined above.
        // ZEROBLOB(n) — n zero bytes.
        "zeroblob" => {
            let n = args.first().map(|v| v.as_integer()).unwrap_or(0).max(0) as usize;
            Value::Blob(vec![0u8; n])
        }
        // CHAR(c1, c2, ...) — construct a string from code points.
        "char" => {
            let mut s = String::new();
            for v in args {
                if let Some(ch) = char::from_u32(v.as_integer() as u32) {
                    s.push(ch);
                }
            }
            Value::Text(s.into())
        }
        // UNICODE(s) — code point of the first character of s.
        "unicode" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                match s.chars().next() {
                    Some(c) => Value::Integer(c as i64),
                    None => Value::Null,
                }
            }
        },
        // TRUE() / FALSE() — SQLite 3.23+ boolean literals.
        "true" => Value::Integer(1),
        "false" => Value::Integer(0),
        // JSON1 — see json.rs. Unknown names return NULL (legacy behavior:
        // unknown functions evaluate to NULL rather than erroring).
        // USER FUNCTIONS take priority over JSON1 so extensions can shadow
        // built-in JSON names (SQLite: user functions override core ones
        // registered in the same "override" slot).
        _ => {
            if let Some(r) = crate::plugin::call_user_scalar(&fname, args) {
                return r;
            }
            crate::executor::json::call_json_function(&fname, args).unwrap_or(Value::Null)
        }
    })
}

/// Built-in scalar/aggregate/window function names (used to reject
/// `create_function` overrides of engine internals, matching SQLite's
/// SQLITE_BUSY error for overwriting a core function). Aggregates are
/// included so user aggregates can't silently shadow them either.
pub(crate) fn is_builtin_scalar(name: &str) -> bool {
    const BUILTIN: &[&str] = &[
        "abs",
        "avg",
        "ceil",
        "ceiling",
        "changes",
        "char",
        "coalesce",
        "count",
        "currentdate",
        "currenttime",
        "currenttimestamp",
        "date",
        "datetime",
        "dense_rank",
        "exp",
        "false",
        "floor",
        "group_concat",
        "hex",
        "ifnull",
        "iif",
        "instr",
        "json",
        "json_array",
        "json_array_length",
        "json_extract",
        "json_insert",
        "json_object",
        "json_patch",
        "json_quote",
        "json_remove",
        "json_replace",
        "json_set",
        "json_valid",
        "julianday",
        "last_insert_rowid",
        "length",
        "ln",
        "log",
        "log10",
        "log2",
        "lower",
        "ltrim",
        "max",
        "min",
        "nullif",
        "pi",
        "power",
        "printf",
        "quote",
        "random",
        "randomblob",
        "rank",
        "replace",
        "round",
        "row_number",
        "rtrim",
        "sign",
        "sqlite_version",
        "sqrt",
        "strftime",
        "substr",
        "substring",
        "sum",
        "time",
        "timediff",
        "total",
        "total_changes",
        "trim",
        "true",
        "trunc",
        "typeof",
        "unicode",
        "unixepoch",
        "upper",
        "zeroblob",
    ];
    let lowered = name.to_ascii_lowercase();
    BUILTIN.binary_search(&lowered.as_str()).is_ok()
}

fn quote_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => crate::types::format_real(*f),
        Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Blob(b) => format!(
            "X'{}'",
            b.iter().map(|x| format!("{:02X}", x)).collect::<String>()
        ),
    }
}

fn rand_i64() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    now.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

/// Extract a COLLATE name from one side of a comparison (SQLite allows
/// `a COLLATE X < b` and `a < b COLLATE X`; the RHS wins when both sides
/// specify one).
fn comparison_collation(e: &Expr) -> Option<String> {
    if let Expr::Collate { collation, .. } = e {
        return Some(collation.clone());
    }
    None
}

/// Comparison through a collation (text-text pairs only; other types keep
/// the engine's total order). Result mirrors `apply_binary` for the six
/// comparison operators.
fn apply_binary_collated(
    op: BinaryOp,
    l: &Value,
    r: &Value,
    coll: &dyn crate::plugin::Collation,
) -> Value {
    use std::cmp::Ordering;
    use BinaryOp::*;
    match op {
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                return Value::Null;
            }
            let ord = crate::plugin::compare_collated(l, r, coll);
            let b = matches!(
                (op, ord),
                (BinaryOp::Eq, Ordering::Equal)
                    | (BinaryOp::NotEq, Ordering::Less | Ordering::Greater)
                    | (BinaryOp::Lt, Ordering::Less)
                    | (BinaryOp::LtEq, Ordering::Less | Ordering::Equal)
                    | (BinaryOp::Gt, Ordering::Greater)
                    | (BinaryOp::GtEq, Ordering::Greater | Ordering::Equal)
            );
            Value::Integer(if b { 1 } else { 0 })
        }
        _ => apply_binary(op, l, r),
    }
}

/// Apply a binary operator.
/// SQL comparisons involving NaN yield NULL (SQLite semantics: NaN is
/// equal to nothing, not even itself — only the `IS` operator treats
/// NaN as identical to NaN).
fn cmp_operand_missing(v: &Value) -> bool {
    v.is_null() || matches!(v, Value::Real(f) if f.is_nan())
}

pub fn apply_binary(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    match op {
        // Integer overflow PROMOTES TO REAL (SQLite: 9223372036854775807 + 1
        // is 9.223372036854776e18, never a wrapped i64).
        Add => arith_checked(l, r, i64::checked_add, |a, b| a + b),
        Sub => arith_checked(l, r, i64::checked_sub, |a, b| a - b),
        Mul => arith_checked(l, r, i64::checked_mul, |a, b| a * b),
        Div => {
            if r.as_integer() == 0 || r.as_real() == 0.0 {
                Value::Null
            } else {
                // i64::MIN / -1 overflows; checked_div promotes to REAL.
                arith_checked(l, r, i64::checked_div, |a, b| a / b)
            }
        }
        Mod => {
            let b = r.as_integer();
            if b == 0 {
                Value::Null
            } else if b == -1 {
                // i64::MIN % -1 overflows in Rust; mathematically 0.
                Value::Integer(0)
            } else {
                Value::Integer(l.as_integer() % b)
            }
        }
        Concat => l.concat(r),
        BitAnd => Value::Integer(l.as_integer() & r.as_integer()),
        BitOr => Value::Integer(l.as_integer() | r.as_integer()),
        BitXor => Value::Integer(l.as_integer() ^ r.as_integer()),
        ShiftLeft => Value::Integer(l.as_integer() << (r.as_integer() & 63)),
        ShiftRight => Value::Integer(l.as_integer() >> (r.as_integer() & 63)),
        // SQL three-valued logic: any comparison with NULL on either side
        // produces NULL (UNKNOWN), which is filtered out by WHERE.
        // Previously we did `if l == r { 1 } else { 0 }`, which — combined
        // with our `PartialEq` treating `Null == Null` as true — caused
        // `WHERE col = NULL` to match every row where col was NULL.
        // This bug was caught by the SLT test suite.
        Eq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l == r { 1 } else { 0 })
            }
        }
        NotEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l != r { 1 } else { 0 })
            }
        }
        Lt => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l < r { 1 } else { 0 })
            }
        }
        LtEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l <= r { 1 } else { 0 })
            }
        }
        Gt => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l > r { 1 } else { 0 })
            }
        }
        GtEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(if l >= r { 1 } else { 0 })
            }
        }
        And => Value::Integer(if l.is_truthy() && r.is_truthy() { 1 } else { 0 }),
        Or => Value::Integer(if l.is_truthy() || r.is_truthy() { 1 } else { 0 }),
    }
}

fn arith_checked<F, I>(l: &Value, r: &Value, fi: F, ff: I) -> Value
where
    F: Fn(i64, i64) -> Option<i64>,
    I: Fn(f64, f64) -> f64,
{
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    if matches!(l, Value::Real(_)) || matches!(r, Value::Real(_)) {
        Value::Real(ff(l.as_real(), r.as_real()))
    } else {
        let (a, b) = (l.as_integer(), r.as_integer());
        match fi(a, b) {
            Some(v) => Value::Integer(v),
            // Integer overflow: SQLite promotes the whole expression to
            // REAL instead of wrapping (`9223372036854775807 + 1` is
            // 9.223372036854776e18, never i64::MIN).
            None => Value::Real(ff(a as f64, b as f64)),
        }
    }
}

pub fn apply_unary(op: UnaryOp, v: &Value) -> Value {
    match op {
        UnaryOp::Neg => {
            if v.is_null() {
                Value::Null
            } else if matches!(v, Value::Real(_)) {
                Value::Real(-v.as_real())
            } else {
                let i = v.as_integer();
                match i.checked_neg() {
                    Some(n) => Value::Integer(n),
                    // -i64::MIN overflows: promote to REAL (SQLite).
                    None => Value::Real(-(i as f64)),
                }
            }
        }
        UnaryOp::Pos => v.clone(),
        UnaryOp::Not => Value::Integer(if v.is_truthy() { 0 } else { 1 }),
        UnaryOp::BitNot => {
            if v.is_null() {
                Value::Null
            } else {
                Value::Integer(!v.as_integer())
            }
        }
    }
}

/// SQLite-style LIKE matching: % matches any sequence, _ matches any single char.
/// Case-insensitive by default.
pub fn like_match(
    value: &Value,
    pattern: &Value,
    escape: Option<&Value>,
    case_sensitive: bool,
) -> bool {
    // Zero-allocation fast path: no ESCAPE clause, both operands TEXT,
    // ASCII pattern. LIKE's case folding is ASCII-only in SQLite
    // (non-ASCII bytes compare exactly), so a byte scan with ASCII
    // folding is semantically identical — and skips the per-row
    // `as_text()` String clone plus the two `Vec<char>` allocations +
    // Unicode lowercase the general path pays. A 100k-row `LIKE '%x%'`
    // scan drops from ~325 ns/row to ~15-30 ns/row.
    if escape.is_none() {
        if let (Value::Text(sv), Value::Text(pv)) = (value, pattern) {
            if pv.is_ascii() {
                if let Some(hit) = like_match_bytes(
                    sv.as_str().as_bytes(),
                    pv.as_str().as_bytes(),
                    case_sensitive,
                ) {
                    return hit;
                }
                // None = general wildcard shape on a NON-ASCII subject
                // (`_` must match one CHARACTER, not one byte): the
                // char-based path below handles it.
            }
        }
    }
    // General path: ESCAPE support, non-TEXT operands (numeric LIKE casts
    // to text), non-ASCII patterns, Unicode folding.
    let s = value.as_text();
    let p = pattern.as_text();
    let esc = escape.map(|v| v.as_text().chars().next().unwrap_or('\\'));
    like_match_str(&s, &p, esc, case_sensitive)
}

/// ASCII case folding helper (SQLite LIKE folds ASCII letters only).
#[inline]
fn fold_ascii(b: u8, case_sensitive: bool) -> u8 {
    if case_sensitive {
        b
    } else {
        b.to_ascii_lowercase()
    }
}

/// Byte-level LIKE for ASCII patterns without ESCAPE. Classifies the
/// pattern shape first (`%x%` / `x%` / `%x` / plain / general) and runs
/// the cheapest matcher that shape allows.
/// Returns `None` when the pattern needs the CHARACTER-level general
/// matcher AND the subject is non-ASCII (`_` must match one UTF-8
/// character, not one byte). Every classified shape (contains / prefix /
/// suffix / equality with an ASCII needle) is byte-safe for any subject.
fn like_match_bytes(s: &[u8], p: &[u8], case_sensitive: bool) -> Option<bool> {
    // Pattern shape classification (no ESCAPE: '%' and '_' are the only
    // metacharacters).
    let lead = p.first() == Some(&b'%');
    let trail = p.last() == Some(&b'%');
    if lead || trail || p.iter().any(|&b| b == b'%' || b == b'_') {
        // strip one leading/trailing '%' and require the rest literal.
        // A one-byte "%" pattern makes start=1,end=0 — clamp so the
        // empty needle falls to the general matcher (which returns true
        // for any subject, the correct semantics).
        let start = usize::from(lead);
        let end = p.len().saturating_sub(usize::from(trail)).max(start);
        let needle = &p[start..end];
        let needle_wild = needle.iter().any(|&b| b == b'%' || b == b'_');
        if lead && trail && !needle_wild && !needle.is_empty() {
            return Some(bytes_contains_fold(s, needle, case_sensitive));
        }
        if !lead && trail && !needle_wild && !needle.is_empty() {
            // `literal%`: prefix compare.
            return Some(
                s.len() >= needle.len()
                    && bytes_eq_fold(&s[..needle.len()], needle, case_sensitive),
            );
        }
        if lead && !trail && !needle_wild && !needle.is_empty() {
            // `%literal`: suffix compare.
            return Some(
                s.len() >= needle.len()
                    && bytes_eq_fold(&s[s.len() - needle.len()..], needle, case_sensitive),
            );
        }
        // General shape (embedded wildcards, empty needles, bare '%'):
        // iterative wildcard matcher — no allocation, no recursion.
        // Non-ASCII subject: `_` semantics need the char path.
        if !s.is_ascii() {
            return None;
        }
        return Some(like_general_bytes(s, p, case_sensitive));
    }
    // No wildcards at all: exact (folded) equality.
    Some(bytes_eq_fold(s, p, case_sensitive))
}

#[inline]
fn bytes_eq_fold(a: &[u8], b: &[u8], case_sensitive: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if case_sensitive {
        a == b
    } else {
        a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.eq_ignore_ascii_case(y))
    }
}

fn bytes_contains_fold(hay: &[u8], needle: &[u8], case_sensitive: bool) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    if case_sensitive {
        // memchr-style two-way scan without the per-byte fold: the
        // case-insensitive path below folds every haystack byte; the
        // sensitive path can use the libc-optimized substring search.
        return memchr_contains(hay, needle);
    }
    let first = fold_ascii(needle[0], case_sensitive);
    'outer: for i in 0..=(hay.len() - n) {
        if fold_ascii(hay[i], case_sensitive) != first {
            continue;
        }
        for j in 1..n {
            if fold_ascii(hay[i + j], case_sensitive) != fold_ascii(needle[j], case_sensitive) {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

/// memchr-grade substring search for the case-sensitive path (GLOB-style
/// or LIKE on already-folded bytes): skip by first byte, compare the
/// tail with a single memcmp. The naive per-byte fold loop above costs
/// ~2-3 ns/byte; this costs ~0.3 ns/byte on modern libc memcmp.
fn memchr_contains(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let first = needle[0];
    let mut i = 0usize;
    let last = hay.len() - n;
    while i <= last {
        // Skip to the next first-byte candidate.
        let off = hay[i..].iter().position(|&b| b == first);
        match off {
            None => return false,
            Some(k) => {
                i += k;
                if i > last {
                    return false;
                }
                if hay[i + 1..i + n] == needle[1..] {
                    return true;
                }
                i += 1;
            }
        }
    }
    false
}

/// Case-insensitive ASCII substring search over bytes (LIKE's folding is
/// ASCII-only, so this is exact for ASCII needles against any subject).
/// Pre-folds the needle once — the caller passes the folded needle — and
/// folds each haystack byte once per candidate position.
pub fn like_contains_bytes(hay: &[u8], needle_folded: &[u8]) -> bool {
    let n = needle_folded.len();
    if n == 0 {
        return true;
    }
    if hay.len() < n {
        return false;
    }
    let first = needle_folded[0];
    let mut i = 0usize;
    let last = hay.len() - n;
    'outer: while i <= last {
        if hay[i].to_ascii_lowercase() != first {
            i += 1;
            continue;
        }
        for j in 1..n {
            if hay[i + j].to_ascii_lowercase() != needle_folded[j] {
                i += 1;
                continue 'outer;
            }
        }
        return true;
    }
    false
}

/// Iterative `%`/`_` wildcard matcher over bytes (`_` matches one BYTE —
/// only used when the pattern classifies as general AND the subject is
/// handled byte-wise; callers route non-ASCII subjects through the
/// char-based path when the pattern contains `_`).
fn like_general_bytes(s: &[u8], p: &[u8], case_sensitive: bool) -> bool {
    let (mut si, mut pi) = (0usize, 0usize);
    let (mut star_pi, mut star_si) = (usize::MAX, 0usize);
    while si < s.len() {
        if pi < p.len() && p[pi] == b'%' {
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < p.len()
            && (p[pi] == b'_'
                || fold_ascii(s[si], case_sensitive) == fold_ascii(p[pi], case_sensitive))
        {
            si += 1;
            pi += 1;
        } else if star_pi != usize::MAX {
            star_si += 1;
            si = star_si;
            pi = star_pi + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'%' {
        pi += 1;
    }
    pi == p.len()
}

fn like_match_str(s: &str, p: &str, esc: Option<char>, case_sensitive: bool) -> bool {
    let s_chars: Vec<char> = if case_sensitive {
        s.chars().collect()
    } else {
        s.to_lowercase().chars().collect()
    };
    let p_chars: Vec<char> = if case_sensitive {
        p.chars().collect()
    } else {
        p.to_lowercase().chars().collect()
    };
    let esc_char = esc.unwrap_or('\0');
    like_match_chars(&s_chars, 0, &p_chars, 0, esc_char)
}

fn like_match_chars(s: &[char], si: usize, p: &[char], pi: usize, esc: char) -> bool {
    if pi >= p.len() {
        return si >= s.len();
    }
    let pc = p[pi];
    if pc == esc && pi + 1 < p.len() {
        let next = p[pi + 1];
        if si < s.len() && s[si] == next {
            return like_match_chars(s, si + 1, p, pi + 2, esc);
        }
        return false;
    }
    match pc {
        '%' => {
            // Match zero or more characters.
            for i in si..=s.len() {
                if like_match_chars(s, i, p, pi + 1, esc) {
                    return true;
                }
            }
            false
        }
        '_' => {
            if si < s.len() {
                like_match_chars(s, si + 1, p, pi + 1, esc)
            } else {
                false
            }
        }
        c => {
            if si < s.len() && s[si] == c {
                like_match_chars(s, si + 1, p, pi + 1, esc)
            } else {
                false
            }
        }
    }
}

/// GLOB matching: case-sensitive, * and ? wildcards, [abc] character classes.
pub fn glob_match(value: &Value, pattern: &Value) -> bool {
    let s: Vec<char> = value.as_text().chars().collect();
    let p: Vec<char> = pattern.as_text().chars().collect();
    glob_match_chars(&s, 0, &p, 0)
}

fn glob_match_chars(s: &[char], si: usize, p: &[char], pi: usize) -> bool {
    if pi >= p.len() {
        return si >= s.len();
    }
    match p[pi] {
        '*' => {
            for i in si..=s.len() {
                if glob_match_chars(s, i, p, pi + 1) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if si < s.len() {
                glob_match_chars(s, si + 1, p, pi + 1)
            } else {
                false
            }
        }
        '[' => {
            // Character class
            if si >= s.len() {
                return false;
            }
            let mut end = pi + 1;
            let mut negate = false;
            if end < p.len() && (p[end] == '!' || p[end] == '^') {
                negate = true;
                end += 1;
            }
            let mut matched = false;
            let class_start = end;
            while end < p.len() && p[end] != ']' {
                end += 1;
            }
            let mut i = class_start;
            while i < end {
                if i + 2 < end && p[i + 1] == '-' {
                    if s[si] >= p[i] && s[si] <= p[i + 2] {
                        matched = true;
                    }
                    i += 3;
                } else {
                    if s[si] == p[i] {
                        matched = true;
                    }
                    i += 1;
                }
            }
            if matched ^ negate {
                glob_match_chars(s, si + 1, p, end + 1)
            } else {
                false
            }
        }
        c => {
            if si < s.len() && s[si] == c {
                glob_match_chars(s, si + 1, p, pi + 1)
            } else {
                false
            }
        }
    }
}

/// printf width/padding: left-align with spaces, or zero-pad numerics
/// (sign before the zeros, C semantics).
fn apply_printf_width(raw: String, minus: bool, zero: bool, width: usize) -> String {
    let len = raw.chars().count();
    if len >= width || width == 0 {
        return raw;
    }
    let pad = " ".repeat(width - len);
    if minus {
        format!("{raw}{pad}")
    } else if zero {
        // Zero-pad after a leading sign.
        let (sign, digits) = match raw.strip_prefix('-') {
            Some(d) => ("-", d.to_string()),
            None => ("", raw.clone()),
        };
        let zeros = "0".repeat(width - len);
        format!("{sign}{zeros}{digits}")
    } else {
        format!("{pad}{raw}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Vec<Value> {
        Vec::new()
    }

    fn named_params() -> HashMap<String, Value> {
        HashMap::new()
    }

    #[test]
    fn arithmetic() {
        let col_names = vec!["a".to_string()];
        let row = vec![Value::Integer(5)];
        let p = params();
        let np = named_params();
        let ctx = EvalContext::new(&row, &col_names, &p, &np);
        assert_eq!(
            evaluate(&parse_expr("a + 1"), &ctx).unwrap(),
            Value::Integer(6)
        );
        assert_eq!(
            evaluate(&parse_expr("a * 2"), &ctx).unwrap(),
            Value::Integer(10)
        );
        assert_eq!(
            evaluate(&parse_expr("-a"), &ctx).unwrap(),
            Value::Integer(-5)
        );
    }

    #[test]
    fn string_functions() {
        let col_names: Vec<String> = vec![];
        let row: Vec<Value> = vec![];
        let p = params();
        let np = named_params();
        let ctx = EvalContext::new(&row, &col_names, &p, &np);
        assert_eq!(
            evaluate(&parse_expr("upper('hello')"), &ctx).unwrap(),
            Value::Text("HELLO".to_string().into())
        );
        assert_eq!(
            evaluate(&parse_expr("length('hello')"), &ctx).unwrap(),
            Value::Integer(5)
        );
        assert_eq!(
            evaluate(&parse_expr("coalesce(NULL, 'x')"), &ctx).unwrap(),
            Value::Text("x".to_string().into())
        );
    }

    #[test]
    fn like_matching() {
        assert!(like_match(
            &Value::Text("hello".into()),
            &Value::Text("h%".into()),
            None,
            false
        ));
        assert!(like_match(
            &Value::Text("hello".into()),
            &Value::Text("h_llo".into()),
            None,
            false
        ));
        assert!(like_match(
            &Value::Text("hello".into()),
            &Value::Text("%llo".into()),
            None,
            false
        ));
        assert!(!like_match(
            &Value::Text("hello".into()),
            &Value::Text("world".into()),
            None,
            false
        ));
        assert!(like_match(
            &Value::Text("HELLO".into()),
            &Value::Text("hello".into()),
            None,
            false
        )); // case-insensitive
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match(
            &Value::Text("hello".into()),
            &Value::Text("h*".into())
        ));
        assert!(glob_match(
            &Value::Text("hello".into()),
            &Value::Text("h?llo".into())
        ));
        assert!(glob_match(
            &Value::Text("hello".into()),
            &Value::Text("[hw]ello".into())
        ));
        assert!(!glob_match(
            &Value::Text("hello".into()),
            &Value::Text("world".into())
        ));
    }

    fn parse_expr(src: &str) -> Expr {
        let stmt = crate::sql::parse(&format!("SELECT {}", src)).unwrap();
        match stmt {
            crate::sql::ast::Statement::Select(s) => {
                if let crate::sql::ast::SelectBody::Simple(ss) = s.body {
                    if let crate::sql::ast::ResultColumn::Expr { expr, .. } = &ss.columns[0] {
                        return expr.clone();
                    }
                }
                panic!("not a simple select");
            }
            _ => panic!("not a select"),
        }
    }
}
