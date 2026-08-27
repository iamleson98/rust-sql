//! Expression evaluator.
//!
//! Evaluates an `Expr` against a row, given a schema (list of column names
//! and a function that maps column refs to values). Built for clarity over
//! speed; a production engine would JIT-compile hot expressions.

use crate::error::{Error, Result};
use crate::sql::ast::*;
use crate::types::{Affinity, Value};
use std::collections::HashMap;

/// A row context: maps column references (table, name) to values.
pub struct EvalContext<'a> {
    /// Per-table column values, indexed by table alias.
    /// The key is the alias (or table name if no alias).
    /// The value is a slice of values for that table's columns.
    pub tables: HashMap<String, &'a [Value]>,
    /// Anonymous row: used when there's exactly one source and column refs
    /// don't qualify the table.
    pub row: &'a [Value],
    /// Column names for the anonymous row (used for unqualified refs).
    pub column_names: &'a [String],
    /// Bound parameters.
    pub params: &'a HashMap<String, Value>,
}

impl<'a> EvalContext<'a> {
    pub fn new(
        row: &'a [Value],
        column_names: &'a [String],
        params: &'a HashMap<String, Value>,
    ) -> Self {
        Self {
            tables: HashMap::new(),
            row,
            column_names,
            params,
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
        Value::Null
    }
}

/// Evaluate an expression in the given context.
pub fn evaluate(expr: &Expr, ctx: &EvalContext<'_>) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Parameter(p) => Ok(ctx.params.get(p).cloned().unwrap_or(Value::Null)),
        Expr::Column { table, name } => Ok(ctx.lookup(table, name)),
        Expr::Binary { op, left, right } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
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
            let in_range = v >= lo && v <= hi;
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
                        return Ok(evaluate(val, ctx)?);
                    }
                } else {
                    let c = evaluate(cond, ctx)?;
                    if c.is_truthy() {
                        return Ok(evaluate(val, ctx)?);
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
        Expr::Subquery(_) => Err(Error::Unsupported(
            "scalar subqueries via evaluator (use executor)",
        )),
        Expr::Exists(_) => Err(Error::Unsupported("EXISTS via evaluator (use executor)")),
        Expr::Cast { expr, type_name } => {
            let v = evaluate(expr, ctx)?;
            let affinity = Affinity::from_declared_type(type_name);
            Ok(affinity.coerce(v))
        }
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
    let found = match source {
        InSource::List(list) => {
            let mut found = false;
            for e in list {
                let candidate = evaluate(e, ctx)?;
                if v == candidate {
                    found = true;
                    break;
                }
            }
            found
        }
        InSource::Subquery(_) | InSource::Table(_) => {
            return Err(Error::Unsupported(
                "IN subquery via evaluator (use executor)",
            ));
        }
    };
    Ok(Value::Integer(if found ^ negated { 1 } else { 0 }))
}

fn evaluate_function(name: &str, args: &[Expr], ctx: &EvalContext<'_>) -> Result<Value> {
    let fname = name.to_ascii_lowercase();
    // Scalar functions only here; aggregates are handled by the Aggregate operator.
    let argvals: Result<Vec<Value>> = args.iter().map(|e| evaluate(e, ctx)).collect();
    let argvals = argvals?;
    Ok(call_scalar(&fname, &argvals))
}

/// Call a scalar SQL function.
pub fn call_scalar(name: &str, args: &[Value]) -> Value {
    let fname = name.to_ascii_lowercase();
    match fname.as_str() {
        "abs" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(Value::Integer(i)) => Value::Integer(i.abs()),
            Some(Value::Real(f)) => Value::Real(f.abs()),
            // Numeric-looking text gets coerced; everything else: SQLite returns 0.
            Some(other) => Value::Integer(other.as_integer().abs()),
        },
        "length" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Integer(v.length()),
        },
        "lower" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_lowercase()),
        },
        "upper" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_uppercase()),
        },
        "trim" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().trim().to_string()),
        },
        "ltrim" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().trim_start().to_string()),
        },
        "rtrim" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().trim_end().to_string()),
        },
        "replace" => {
            if args.len() == 3 && args.iter().all(|v| !v.is_null()) {
                let s = args[0].as_text();
                let from = args[1].as_text();
                let to = args[2].as_text();
                Value::Text(s.replace(&from, &to))
            } else {
                Value::Null
            }
        }
        "substr" | "substring" => {
            if args.len() >= 2 && args.iter().take(2).all(|v| !v.is_null())
                && (args.len() < 3 || !args[2].is_null())
            {
                let s = args[0].as_text();
                let start = args[1].as_integer();
                if start <= 0 {
                    Value::Text(s[..((start.unsigned_abs() as usize).min(s.len()))].to_string())
                } else {
                    let start = (start - 1) as usize;
                    if args.len() == 3 {
                        let len = args[2].as_integer() as usize;
                        Value::Text(s[start..(start + len).min(s.len())].to_string())
                    } else {
                        Value::Text(s[start..].to_string())
                    }
                }
            } else {
                Value::Null
            }
        }
        "coalesce" | "ifnull" => {
            for v in args {
                if !v.is_null() {
                    return v.clone();
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
            if args.is_empty() {
                Value::Null
            } else {
                let x = args[0].as_real();
                let n = args.get(1).map(|v| v.as_integer()).unwrap_or(0);
                let factor = 10f64.powi(n as i32);
                Value::Real((x * factor).round() / factor)
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
        "hex" => {
            let s = args.first().map(|v| v.as_text()).unwrap_or_default();
            Value::Text(s.bytes().map(|b| format!("{:02X}", b)).collect())
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
            .to_string(),
        ),
        "date" | "time" | "datetime" | "strftime" | "julianday" => {
            // Minimal date support: just return current time.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            match fname.as_str() {
                "date" => Value::Text(format!("1970-01-01")),
                "time" => Value::Text(format!(
                    "{:02}:{:02}:{:02}",
                    (now / 3600) % 24,
                    (now / 60) % 60,
                    now % 60
                )),
                "datetime" => Value::Text(format!(
                    "1970-01-01 {:02}:{:02}:{:02}",
                    (now / 3600) % 24,
                    (now / 60) % 60,
                    now % 60
                )),
                "julianday" => Value::Real(2440587.5 + now as f64 / 86400.0),
                _ => Value::Null,
            }
        }
        "current_date" => Value::Text("1970-01-01".to_string()),
        "current_time" => Value::Text("00:00:00".to_string()),
        "current_timestamp" => Value::Text("1970-01-01 00:00:00".to_string()),
        "unixepoch" => Value::Integer(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        ),
        "last_insert_rowid" => Value::Integer(0), // overridden by executor
        "changes" => Value::Integer(0),
        "total_changes" => Value::Integer(0),
        "sqlite_version" => Value::Text("3.0.0".to_string()),
        "quote" => {
            let v = args.first().cloned().unwrap_or(Value::Null);
            Value::Text(quote_value(&v))
        }
        _ => Value::Null,
    }
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

/// Apply a binary operator.
pub fn apply_binary(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    match op {
        Add => arith(l, r, |a, b| a + b, |a, b| a + b),
        Sub => arith(l, r, |a, b| a - b, |a, b| a - b),
        Mul => arith(l, r, |a, b| a * b, |a, b| a * b),
        Div => {
            if r.as_integer() == 0 || r.as_real() == 0.0 {
                Value::Null
            } else {
                arith(l, r, |a, b| a / b, |a, b| a / b)
            }
        }
        Mod => {
            let b = r.as_integer();
            if b == 0 {
                Value::Null
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
        Eq => Value::Integer(if l == r { 1 } else { 0 }),
        NotEq => Value::Integer(if l != r { 1 } else { 0 }),
        Lt => Value::Integer(if l < r { 1 } else { 0 }),
        LtEq => Value::Integer(if l <= r { 1 } else { 0 }),
        Gt => Value::Integer(if l > r { 1 } else { 0 }),
        GtEq => Value::Integer(if l >= r { 1 } else { 0 }),
        And => Value::Integer(if l.is_truthy() && r.is_truthy() { 1 } else { 0 }),
        Or => Value::Integer(if l.is_truthy() || r.is_truthy() { 1 } else { 0 }),
    }
}

fn arith<I, F>(l: &Value, r: &Value, fi: F, ff: I) -> Value
where
    F: Fn(i64, i64) -> i64,
    I: Fn(f64, f64) -> f64,
{
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    if matches!(l, Value::Real(_)) || matches!(r, Value::Real(_)) {
        Value::Real(ff(l.as_real(), r.as_real()))
    } else {
        Value::Integer(fi(l.as_integer(), r.as_integer()))
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
                Value::Integer(-v.as_integer())
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
    let s = value.as_text();
    let p = pattern.as_text();
    let esc = escape.map(|v| v.as_text().chars().next().unwrap_or('\\'));
    like_match_str(&s, &p, esc, case_sensitive)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> HashMap<String, Value> {
        HashMap::new()
    }

    #[test]
    fn arithmetic() {
        let col_names = vec!["a".to_string()];
        let row = vec![Value::Integer(5)];
        let p = params();
        let ctx = EvalContext::new(&row, &col_names, &p);
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
        let ctx = EvalContext::new(&row, &col_names, &p);
        assert_eq!(
            evaluate(&parse_expr("upper('hello')"), &ctx).unwrap(),
            Value::Text("HELLO".to_string())
        );
        assert_eq!(
            evaluate(&parse_expr("length('hello')"), &ctx).unwrap(),
            Value::Integer(5)
        );
        assert_eq!(
            evaluate(&parse_expr("coalesce(NULL, 'x')"), &ctx).unwrap(),
            Value::Text("x".to_string())
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
