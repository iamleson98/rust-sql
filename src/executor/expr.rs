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
                if v == candidate {
                    found = true;
                    // Keep iterating to detect NULLs (we need list_has_null
                    // accurate even after a match, for the negated case).
                }
            }
            (found, list_has_null)
        }
        InSource::Subquery(_) | InSource::Table(_) => {
            return Err(Error::Unsupported(
                "IN subquery via evaluator (use executor)",
            ));
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
        "date" | "time" | "datetime" | "strftime" | "julianday" | "unixepoch" | "timediff" => {
            // Full SQLite-compatible date/time engine (see datetime.rs).
            crate::executor::datetime::call_datetime_function(&fname, args)
        }
        "current_date" | "current_time" | "current_timestamp" => {
            crate::executor::datetime::call_datetime_function(&fname, args)
        }
        "last_insert_rowid" => Value::Integer(0), // overridden by executor
        "changes" => Value::Integer(crate::executor::change_counters::last()),
        "total_changes" => Value::Integer(crate::executor::change_counters::total()),
        "sqlite_version" => Value::Text("3.0.0".to_string()),
        "quote" => {
            let v = args.first().cloned().unwrap_or(Value::Null);
            Value::Text(quote_value(&v))
        }
        // INSTR(s, sub) — returns the 1-indexed position of `sub` in `s`,
        // or 0 if not found. NULL inputs return NULL.
        "instr" => {
            if args.len() != 2 || args[0].is_null() || args[1].is_null() {
                return Value::Null;
            }
            let s = args[0].as_text();
            let sub = args[1].as_text();
            if sub.is_empty() {
                return Value::Integer(1);
            }
            match s.find(&sub) {
                Some(pos) => Value::Integer((pos + 1) as i64),
                None => Value::Integer(0),
            }
        }
        // PRINTF — minimal SQLite printf implementation. Supports %d, %s,
        // %f, %x, %c, %% substitutions. NULL format returns NULL.
        "printf" => {
            if args.is_empty() || args[0].is_null() {
                return Value::Null;
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
                // Consume format spec until a conversion char.
                let mut spec = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphabetic() && "diouxXeEfFgGcs%".contains(next) {
                        chars.next();
                        spec.push(next);
                        break;
                    }
                    chars.next();
                    spec.push(next);
                }
                if spec.is_empty() {
                    out.push('%');
                    continue;
                }
                let conv = spec.chars().last().unwrap();
                let arg = args.get(arg_idx).cloned().unwrap_or(Value::Null);
                arg_idx += 1;
                match conv {
                    '%' => out.push('%'),
                    'd' | 'i' => out.push_str(&arg.as_integer().to_string()),
                    'u' => out.push_str(&(arg.as_integer() as u64).to_string()),
                    'x' => out.push_str(&format!("{:x}", arg.as_integer() as u64)),
                    'X' => out.push_str(&format!("{:X}", arg.as_integer() as u64)),
                    'o' => out.push_str(&format!("{:o}", arg.as_integer() as u64)),
                    'f' | 'F' => out.push_str(&format!("{:.*}", 6, arg.as_real())),
                    'e' | 'E' => out.push_str(&format!("{:e}", arg.as_real())),
                    'g' | 'G' => out.push_str(&format!("{}", arg.as_real())),
                    's' => out.push_str(&arg.as_text()),
                    'c' => {
                        let n = arg.as_integer() as u32;
                        if let Some(ch) = char::from_u32(n) {
                            out.push(ch);
                        }
                    }
                    _ => {
                        // Unknown conversion: emit verbatim.
                        out.push('%');
                        out.push_str(&spec);
                    }
                }
            }
            Value::Text(out)
        }
        // MIN(a, b, c, ...) — scalar form (not the aggregate form).
        // Returns the smallest argument. SQLite semantics: if ANY arg is
        // NULL, the result is NULL (the comparison short-circuits).
        "min" if args.len() > 1 => {
            if args.iter().any(|v| v.is_null()) {
                return Value::Null;
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
                return Value::Null;
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
        // POWER(x, y) — x^y.
        "power" => {
            if args.len() == 2 && !args[0].is_null() && !args[1].is_null() {
                Value::Real(args[0].as_real().powf(args[1].as_real()))
            } else {
                Value::Null
            }
        }
        // SQRT(x).
        "sqrt" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x < 0.0 { Value::Null } else { Value::Real(x.sqrt()) }
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
                if x <= 0.0 { Value::Null } else { Value::Real(x.ln()) }
            }
        },
        // LOG(x) / LOG10(x) — base-10 log. With two args, LOG(b, x) is base-b.
        "log" | "log10" => {
            if args.is_empty() || args[0].is_null() {
                return Value::Null;
            }
            if args.len() == 2 && !args[1].is_null() {
                let b = args[0].as_real();
                let x = args[1].as_real();
                if b <= 0.0 || b == 1.0 || x <= 0.0 {
                    return Value::Null;
                }
                return Value::Real(x.log(b));
            }
            let x = args[0].as_real();
            if x <= 0.0 { Value::Null } else { Value::Real(x.log10()) }
        }
        // LOG2(x) — base-2 log.
        "log2" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x <= 0.0 { Value::Null } else { Value::Real(x.log2()) }
            }
        },
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
            Value::Text(s)
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
        }
        // TRUE() / FALSE() — SQLite 3.23+ boolean literals.
        "true" => Value::Integer(1),
        "false" => Value::Integer(0),
        // JSON1 — see json.rs. Unknown names return NULL (legacy behavior:
        // unknown functions evaluate to NULL rather than erroring).
        _ => crate::executor::json::call_json_function(&fname, args)
            .unwrap_or(Value::Null),
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
        // SQL three-valued logic: any comparison with NULL on either side
        // produces NULL (UNKNOWN), which is filtered out by WHERE.
        // Previously we did `if l == r { 1 } else { 0 }`, which — combined
        // with our `PartialEq` treating `Null == Null` as true — caused
        // `WHERE col = NULL` to match every row where col was NULL.
        // This bug was caught by the SLT test suite.
        Eq => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l == r { 1 } else { 0 })
            }
        }
        NotEq => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l != r { 1 } else { 0 })
            }
        }
        Lt => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l < r { 1 } else { 0 })
            }
        }
        LtEq => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l <= r { 1 } else { 0 })
            }
        }
        Gt => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l > r { 1 } else { 0 })
            }
        }
        GtEq => {
            if l.is_null() || r.is_null() {
                Value::Null
            } else {
                Value::Integer(if l >= r { 1 } else { 0 })
            }
        }
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
