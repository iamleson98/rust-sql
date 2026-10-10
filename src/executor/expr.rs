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

/// `x -> path` → the JSON text of the target; `x ->> path` → the SQL
/// value. RHS: INTEGER = array index, TEXT '$...' = full path, TEXT
/// '[N]' = index, other TEXT = a single object key. A missing target is
/// NULL; malformed documents/paths RAISE like SQLite.
fn arrow_eval(op: BinaryOp, l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let doc = match crate::executor::json::load_doc(l)? {
        Some(d) => d,
        None => return Ok(Value::Null),
    };
    let segs = crate::executor::jsonb::arrow_rhs_segments(r).map_err(Error::Runtime)?;
    let Some(segs) = segs else {
        return Ok(Value::Null);
    };
    let node = crate::executor::jsonb::jb_resolve(&doc, &segs);
    Ok(match (op, node) {
        (BinaryOp::Arrow, Some(n)) => Value::Text(n.render().into()),
        (BinaryOp::ArrowText, Some(n)) => n.to_value(),
        _ => Value::Null,
    })
}

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
/// the integer 12). `CAST(x AS BOOLEAN)` is PostgreSQL-borrowed: the PG
/// boolean literal words map to 1/0 (see the arm below).
/// Internal "affinity carrier" type names (never produced by the parser —
/// they start with a NUL). An uncorrelated subquery materialized into a
/// literal at execution time keeps the comparison affinity its result
/// column had (SQLite's sqlite3ExprAffinity(TK_SELECT)) by wrapping the
/// literal in `CAST(v AS <marker>)`, which evaluates as the IDENTITY.
///
/// * `\0aff:<A>`   — the expression's own affinity is `<A>` (scalar
///   subquery result);
/// * `\0inrhs:<A>` — on an IN operand: the materialized `IN (SELECT …)`
///   list's column affinity was `<A>` (exprINAffinity combines it with
///   the operand's own via sqlite3CompareAffinity).
pub(crate) const AFF_MARKER: &str = "\0aff:";
pub(crate) const IN_RHS_MARKER: &str = "\0inrhs:";
/// Identity carrier around a MATERIALIZED uncorrelated subquery value with
/// no affinity of its own: the expression still "holds a subquery"
/// (EP_Subquery) for the evaluation-order rules (`contains_subquery`).
/// AFF_MARKER and IN_RHS_MARKER carriers wrap materialized subqueries too.
pub(crate) const SUBQ_MARKER: &str = "\0subq";

pub(crate) fn affinity_marker(prefix: &str, a: crate::types::Affinity) -> String {
    use crate::types::Affinity as A;
    let tag = match a {
        A::Integer => "INTEGER",
        A::Real => "REAL",
        A::Numeric => "NUMERIC",
        A::Text => "TEXT",
        A::Blob | A::None => "BLOB",
    };
    format!("{prefix}{tag}")
}

pub(crate) fn marker_affinity(type_name: &str, prefix: &str) -> Option<crate::types::Affinity> {
    type_name
        .strip_prefix(prefix)
        .map(crate::types::Affinity::from_declared_type)
}

fn cast_value(v: Value, type_name: &str) -> Value {
    if type_name.starts_with('\0') {
        return v; // internal affinity carrier: identity
    }
    // PostgreSQL CAST AS BOOLEAN: 'true'/'false'/'t'/'f'/'yes'/'no'/
    // 'on'/'off'/'1'/'0' (case-insensitive, surrounding whitespace
    // allowed) map to 1/0; numbers map by truthiness. Unrecognized text
    // falls through to the SQLite NUMERIC cast below (numeric prefix →
    // 0), so `CAST('maybe' AS BOOLEAN)` is 0, not an error — the engine's
    // CAST is infallible by design (SQLite semantics).
    {
        let t = type_name.trim().to_ascii_uppercase();
        if t == "BOOLEAN" || t == "BOOL" {
            return match v {
                Value::Null => Value::Null,
                Value::Integer(i) => Value::Integer((i != 0) as i64),
                Value::Real(f) => Value::Integer((f != 0.0) as i64),
                Value::Text(_) | Value::Blob(_) => {
                    let s = v.as_text().trim().to_ascii_lowercase();
                    match s.as_str() {
                        "true" | "t" | "yes" | "on" | "1" => Value::Integer(1),
                        "false" | "f" | "no" | "off" | "0" => Value::Integer(0),
                        _ => {
                            // SQLite NUMERIC-cast fallback (numeric prefix)
                            let f = crate::types::value::parse_real_prefix(&v.as_text());
                            if f.is_finite()
                                && f.trunc() == f
                                && f.abs() <= 9.007_199_254_740_992e15
                            {
                                Value::Integer(f as i64)
                            } else {
                                Value::Real(f)
                            }
                        }
                    }
                }
            };
        }
    }
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
            Value::Text(_) | Value::Blob(_) => {
                Value::Real(crate::types::value::parse_real_prefix(&v.as_text()))
            }
        },
        Affinity::Text => match v {
            Value::Null => Value::Null,
            other => Value::Text(other.as_text().into()),
        },
        Affinity::Blob => match v {
            Value::Null => Value::Null,
            Value::Blob(b) => Value::Blob(b),
            other => {
                // SQLite's CAST(text AS BLOB) yields the DATABASE
                // ENCODING's bytes — on a UTF-16 file that is the UTF-16
                // code units, not the engine's UTF-8. The connection's
                // encoding rides the statement-entry TLS (see
                // executor::conn_enc); parallel workers never evaluate
                // CAST, so the tag is always current here.
                let text = other.as_text();
                let enc = crate::executor::conn_enc::current();
                if enc == crate::storage::sqlitefmt::record::TextEnc::Utf8 {
                    Value::Blob(text.into_bytes())
                } else {
                    Value::Blob(crate::storage::sqlitefmt::record::text_encoded_bytes(
                        text.as_str(),
                        enc,
                    ))
                }
            }
        },
        // NUMERIC cast: longest numeric PREFIX (CAST semantics, not
        // affinity — pinned via examples/probe_numeric_affinity.rs):
        // `CAST('123' AS NUMERIC)` is INTEGER 123, `CAST('1.5' AS NUMERIC)`
        // is REAL 1.5, `CAST('abc' AS NUMERIC)` is INTEGER 0, and — unlike
        // column affinity — `CAST(12.0 AS NUMERIC)` stays REAL 12.0 (CAST
        // never squeezes a REAL down to INTEGER).
        Affinity::Numeric => match v {
            Value::Null => Value::Null,
            Value::Integer(i) => Value::Integer(i),
            Value::Real(f) => Value::Real(f),
            // vdbemem.c sqlite3VdbeMemNumerify, verbatim: an integer
            // reading (sqlite3Atoi64, trailing text allowed) when the
            // text is not a well-formed REAL; else the REAL, squeezed to
            // INTEGER only when sqlite3RealSameAsInt. (A float-only
            // reading lost exact integers past 2^53:
            // CAST('-9223372036854775807' AS NUMERIC) came back REAL.)
            Value::Text(_) | Value::Blob(_) => {
                let bytes: Vec<u8> = match &v {
                    Value::Blob(b) => b.clone(),
                    other => other.as_text().as_bytes().to_vec(),
                };
                let (r, rc) = crate::types::numeric::atof(&bytes);
                let (ix, irc) = crate::types::numeric::atoi64(&bytes);
                if (rc == 0 || rc == 1) && irc <= 1 {
                    Value::Integer(ix)
                } else {
                    let ix = crate::types::numeric::real_to_i64(r);
                    if crate::types::numeric::real_same_as_int(r, ix) {
                        Value::Integer(ix)
                    } else {
                        Value::Real(r)
                    }
                }
            }
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
    /// Column affinities parallel to `column_names`, when the row is a
    /// single table's row (scan filters, residuals, projections). None
    /// for combined/join rows and legacy contexts — comparisons then keep
    /// the engine's raw total order (no affinity coercion). This drives
    /// SQLite's comparison-affinity rules: a column operand lends its
    /// affinity to a literal/parameter operand before comparing
    /// (`text_col > 0` compares TEXT-to-TEXT; `num_col > '5'` converts
    /// '5' to 5; `num_col > 'abc'` stays a cross-type comparison because
    /// NUMERIC affinity only converts numeric-LOOKING text).
    pub column_affinities: Option<&'a [crate::types::Affinity]>,
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
            column_affinities: None,
        }
    }

    /// Attach the column-affinity map (single-table row contexts).
    pub fn with_affinities(mut self, affinities: Option<&'a [crate::types::Affinity]>) -> Self {
        self.column_affinities = affinities;
        self
    }

    /// The affinity a column-reference expression resolves to in this
    /// context: `Some(aff)` for a real column (or the rowid pseudo-column
    /// — INTEGER), `None` when the context has no affinity map or the
    /// name resolves outside this row (conservative: the comparison then
    /// keeps raw semantics). Mirrors `lookup`'s local resolution order.
    pub fn column_expr_affinity(&self, e: &Expr) -> Option<crate::types::Affinity> {
        let Expr::Column { table, name } = e else {
            return None;
        };
        if table.is_some() {
            // Qualified refs in single-table contexts match the
            // "t.c"-qualified column list; the hidden rowid slot carries
            // INTEGER affinity.
            let t = table.as_deref().unwrap();
            let qual = format!("{}.{}", t.to_ascii_lowercase(), name.to_ascii_lowercase());
            for (i, n) in self.column_names.iter().enumerate() {
                if n.to_ascii_lowercase() == qual {
                    return self.affinity_at(i);
                }
            }
            if crate::planner::is_rowid_spelling(name) {
                for n in self.column_names.iter() {
                    if crate::planner::is_hidden_rowid(n)
                        && n[..n.rfind('.')?].eq_ignore_ascii_case(t)
                    {
                        return Some(crate::types::Affinity::Integer);
                    }
                }
            }
            return None;
        }
        // Bare name: exact, then qualified-suffix ("t.c" for "c"), then
        // the rowid spellings (real column first — `position` order).
        for (i, n) in self.column_names.iter().enumerate() {
            if n.eq_ignore_ascii_case(name) {
                return self.affinity_at(i);
            }
        }
        for (i, n) in self.column_names.iter().enumerate() {
            if let Some(pos) = n.rfind('.') {
                if n[pos + 1..].eq_ignore_ascii_case(name) {
                    return self.affinity_at(i);
                }
            }
        }
        if crate::planner::is_rowid_spelling(name)
            && self
                .column_names
                .iter()
                .any(|c| c.eq_ignore_ascii_case("rowid") || crate::planner::is_hidden_rowid(c))
        {
            return Some(crate::types::Affinity::Integer);
        }
        None
    }

    fn affinity_at(&self, i: usize) -> Option<crate::types::Affinity> {
        self.column_affinities?.get(i).copied()
    }

    /// Comparison-affinity resolution for a column reference: the
    /// context's own affinity map when it has one, otherwise the
    /// statement-scoped source registry ([`AffinityScope`]) — which is
    /// what gives projection lists, GROUP BY / HAVING / ORDER BY
    /// expressions, aggregate arguments and join rows the same
    /// comparison semantics as single-table WHERE filters.
    pub fn comparison_column_affinity(&self, e: &Expr) -> Option<crate::types::Affinity> {
        if self.column_affinities.is_some() {
            return self.column_expr_affinity(e);
        }
        let Expr::Column { table, name } = e else {
            return None;
        };
        affinity_scope_lookup(
            self.source_qualifier(table.as_deref(), name).as_deref(),
            name,
        )
    }

    /// The FROM qualifier a column reference binds to: its own, else the
    /// one this row's column list resolves a bare name to (so the
    /// registry lookup learns which source it means).
    fn source_qualifier(&self, table: Option<&str>, name: &str) -> Option<String> {
        match table {
            Some(t) => Some(t.to_ascii_lowercase()),
            None => {
                for n in self.column_names.iter() {
                    if n.eq_ignore_ascii_case(name) {
                        return None;
                    }
                    if let Some(pos) = n.rfind('.') {
                        if n[pos + 1..].eq_ignore_ascii_case(name) {
                            return Some(n[..pos].to_ascii_lowercase());
                        }
                    }
                }
                None
            }
        }
    }

    /// sqlite3ExprCollSeq for an operand: an explicit COLLATE (also one
    /// nested inside the expression — SQLite's EP_Collate propagation),
    /// else — through CAST and unary `+` — a table column's DECLARED
    /// collation ("BINARY" when it declares none). `None`: no collation
    /// (the comparison's other operand may supply one).
    pub(crate) fn expr_collation(&self, e: &Expr) -> Option<String> {
        if let Some(c) = explicit_collation(e) {
            return Some(c);
        }
        // No collated column anywhere in scope: a column operand is BINARY,
        // which is the default — nothing to resolve.
        if COLLATED_FRAMES.with(|c| c.get()) == 0 {
            return None;
        }
        let mut p = e;
        loop {
            match p {
                Expr::Cast { expr, .. }
                | Expr::Unary {
                    op: crate::sql::ast::UnaryOp::Pos,
                    expr,
                } => p = expr,
                Expr::Column { table, name } => {
                    let q = self.source_qualifier(table.as_deref(), name);
                    return collation_scope_lookup(q.as_deref(), name);
                }
                _ => return None,
            }
        }
    }

    /// sqlite3ExprCollSeq for a SQLITE_FUNC_NEEDCOLL argument (scalar
    /// min / max, nullif — "the first argument that has a collation"):
    /// like [`Self::expr_collation`] but without its no-collated-scope
    /// shortcut, because here a plain column DEFINES one (BINARY) and
    /// stops the search — `nullif(j, upper(j COLLATE NOCASE))` compares
    /// BINARY. The rowid defines none.
    pub(crate) fn needcoll_arg_collation(&self, e: &Expr) -> Option<String> {
        if let Some(c) = explicit_collation(e) {
            return Some(c);
        }
        let mut p = e;
        loop {
            match p {
                Expr::Cast { expr, .. }
                | Expr::Unary {
                    op: crate::sql::ast::UnaryOp::Pos,
                    expr,
                } => p = expr,
                Expr::Column { table, name } => {
                    let q = self.source_qualifier(table.as_deref(), name);
                    return match collation_scope_lookup_ex(q.as_deref(), name) {
                        Some(found) => found,
                        None if crate::planner::is_rowid_spelling(name) => None,
                        None => Some("BINARY".to_string()),
                    };
                }
                _ => return None,
            }
        }
    }

    /// sqlite3BinaryCompareCollSeq: an explicit COLLATE on the left
    /// operand, else on the right, else the left operand's collation
    /// (a left COLUMN decides even when it is the default BINARY), else
    /// the right's. Comparisons in EVERY context get this — the
    /// planner's WHERE/ON rewrite used to be the only place a column's
    /// declared collation applied (`SELECT b = 'X'` over a NOCASE column
    /// compared BINARY).
    pub(crate) fn binary_comparison_collation(&self, left: &Expr, right: &Expr) -> Option<String> {
        explicit_collation(left)
            .or_else(|| explicit_collation(right))
            .or_else(|| self.expr_collation(left))
            .or_else(|| self.expr_collation(right))
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
            // Qualified rowid spelling that matched no real column: the
            // side's hidden rowid slot, named "<prefix>.\0rowid". This
            // is what makes `a.rowid` / `b.rowid` bind to the CORRECT
            // side in a join's combined column list (both sides carry a
            // trailing slot; a bare-name fallback would always bind the
            // first one).
            if crate::planner::is_rowid_spelling(name) {
                for (i, n) in self.column_names.iter().enumerate() {
                    if crate::planner::is_hidden_rowid(n) {
                        if let Some(pos) = n.rfind('.') {
                            if n[..pos].eq_ignore_ascii_case(t) {
                                return self.row.get(i).cloned().unwrap_or(Value::Null);
                            }
                        }
                    }
                }
            }
            // Hidden vtab aux slot (FTS5's `rank` / table-self column):
            // the marked name `t.\u{1}rank` answers `t.rank` after every
            // real column failed.
            for (i, n) in self.column_names.iter().enumerate() {
                if crate::executor::vtab_exec::aux_slot_matches(n, Some(t), name) {
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
        // Special column: rowid / _rowid_ / oid — and the planner's
        // rewritten hidden name (no-alias tables; see
        // planner::HIDDEN_ROWID). The trailing pseudo-rowid slot may be
        // registered under EITHER name (legacy eval contexts append
        // "rowid", driver output columns append the hidden name):
        // match both — including the side-qualified "a.\0rowid"
        // spelling (first slot wins = leftmost source, SQLite's
        // left-to-right resolution for bare names).
        if name.eq_ignore_ascii_case("rowid")
            || name.eq_ignore_ascii_case("_rowid_")
            || name.eq_ignore_ascii_case("oid")
            || name == crate::planner::HIDDEN_ROWID
        {
            if let Some(idx) = self
                .column_names
                .iter()
                .position(|c| c.eq_ignore_ascii_case("rowid") || crate::planner::is_hidden_rowid(c))
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
        // Hidden vtab aux slot (FTS5's `rank` / table-self column): the
        // marked name answers the bare reference after every real column
        // failed.
        for (i, n) in self.column_names.iter().enumerate() {
            if crate::executor::vtab_exec::aux_slot_matches(n, None, name) {
                return self.row.get(i).cloned().unwrap_or(Value::Null);
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

// ---------------------------------------------------------------------------
// Statement-scoped affinity registry
// ---------------------------------------------------------------------------

/// One frame of the affinity registry: FROM qualifier -> table schema.
type AffinityFrame = Vec<(String, std::sync::Arc<crate::schema::Table>)>;

std::thread_local! {
    /// How many registry frames hold a column with a non-BINARY declared
    /// collation. Zero (the common case) means every column reference
    /// resolves to BINARY: `expr_collation` then skips the per-comparison
    /// registry walk entirely.
    static COLLATED_FRAMES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Stack of source frames (innermost last): each frame maps a FROM
    /// qualifier (alias, else table name — lowercase) to its table schema.
    static AFFINITY_SCOPES: std::cell::RefCell<Vec<AffinityFrame>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// RAII frame of the affinity registry (see
/// [`EvalContext::comparison_column_affinity`]). Pushed by the executor
/// around each plan subtree's execution.
pub(crate) struct AffinityScope {
    pushed: bool,
    /// This frame holds a column with a non-BINARY declared collation.
    collated: bool,
}

impl AffinityScope {
    pub(crate) fn push(sources: Vec<(String, std::sync::Arc<crate::schema::Table>)>) -> Self {
        if sources.is_empty() {
            return AffinityScope {
                pushed: false,
                collated: false,
            };
        }
        let collated = sources.iter().any(|(_, t)| {
            t.columns
                .iter()
                .any(|c| !c.collation.is_empty() && !c.collation.eq_ignore_ascii_case("BINARY"))
        });
        if collated {
            COLLATED_FRAMES.with(|c| c.set(c.get() + 1));
        }
        AFFINITY_SCOPES.with(|s| s.borrow_mut().push(sources));
        AffinityScope {
            pushed: true,
            collated,
        }
    }
}

impl Drop for AffinityScope {
    fn drop(&mut self) {
        if self.pushed {
            AFFINITY_SCOPES.with(|s| {
                s.borrow_mut().pop();
            });
        }
        if self.collated {
            COLLATED_FRAMES.with(|c| c.set(c.get() - 1));
        }
    }
}

/// Resolve a column's declared affinity through the registry, innermost
/// frame first. A bare name binds to the first source (left to right)
/// that has such a column; rowid spellings are INTEGER.
fn affinity_scope_lookup(qualifier: Option<&str>, name: &str) -> Option<crate::types::Affinity> {
    AFFINITY_SCOPES.with(|s| {
        let s = s.borrow();
        for frame in s.iter().rev() {
            for (q, table) in frame.iter() {
                if let Some(want) = qualifier {
                    if q != want {
                        continue;
                    }
                }
                if let Some(i) = table.find_column(name) {
                    return table.columns.get(i).map(|c| c.affinity);
                }
                if crate::planner::is_rowid_spelling(name) && !table.without_rowid {
                    return Some(crate::types::Affinity::Integer);
                }
                if qualifier.is_some() {
                    return None;
                }
            }
        }
        None
    })
}

/// A column's DECLARED collation through the registry (see
/// `affinity_scope_lookup` for the binding rules): `Some("BINARY")` for
/// a column declaring none and for the rowid; `None` when the reference
/// is not a table column in scope.
fn collation_scope_lookup(qualifier: Option<&str>, name: &str) -> Option<String> {
    collation_scope_lookup_ex(qualifier, name).flatten()
}

/// [`collation_scope_lookup`] telling "resolved, no collation" (the
/// rowid: `Some(None)`) apart from "not resolved in any scope" (`None`).
fn collation_scope_lookup_ex(qualifier: Option<&str>, name: &str) -> Option<Option<String>> {
    AFFINITY_SCOPES.with(|s| {
        let s = s.borrow();
        for frame in s.iter().rev() {
            for (q, table) in frame.iter() {
                if let Some(want) = qualifier {
                    if q != want {
                        continue;
                    }
                }
                // The rowid — an INTEGER PRIMARY KEY column or a rowid
                // spelling — defines NO collation: SQLite resolves both to
                // iColumn -1 and sqlite3ExprCollSeq skips them, so the next
                // operand decides (`max('NULL', id, h)` over a NOCASE `h`
                // compares NOCASE; `id = h` takes h's collation).
                if let Some(i) = table.find_column(name) {
                    if table.rowid_alias == Some(i) {
                        return Some(None);
                    }
                    return Some(table.columns.get(i).map(|c| {
                        if c.collation.is_empty() {
                            "BINARY".to_string()
                        } else {
                            c.collation.clone()
                        }
                    }));
                }
                if crate::planner::is_rowid_spelling(name) && !table.without_rowid {
                    return Some(None);
                }
                if qualifier.is_some() {
                    return None;
                }
            }
        }
        None
    })
}

/// An EXPLICIT `COLLATE` within an operand, SQLite-style: the EP_Collate
/// flag propagates up from a COLLATE node through its ancestors, and
/// sqlite3ExprCollSeq follows it down (left child first, then right,
/// then list arguments). Subqueries are their own scope.
pub(crate) fn explicit_collation(e: &Expr) -> Option<String> {
    match e {
        Expr::Collate { collation, .. } => Some(collation.clone()),
        Expr::Cast { expr, .. } | Expr::Unary { expr, .. } => explicit_collation(expr),
        Expr::Binary { left, right, .. } => {
            explicit_collation(left).or_else(|| explicit_collation(right))
        }
        Expr::Function { args, .. } => args.iter().find_map(explicit_collation),
        Expr::Case {
            operand,
            whens,
            else_,
        } => operand
            .as_deref()
            .and_then(explicit_collation)
            .or_else(|| {
                whens
                    .iter()
                    .find_map(|(w, t)| explicit_collation(w).or_else(|| explicit_collation(t)))
            })
            .or_else(|| else_.as_deref().and_then(explicit_collation)),
        // TK_BETWEEN / TK_IN: the left operand, then the x.pList members.
        Expr::Between {
            expr, low, high, ..
        } => explicit_collation(expr)
            .or_else(|| explicit_collation(low))
            .or_else(|| explicit_collation(high)),
        Expr::In { expr, source, .. } => explicit_collation(expr).or_else(|| match source {
            crate::sql::ast::InSource::List(items) => items.iter().find_map(explicit_collation),
            _ => None,
        }),
        // LIKE / GLOB are the function like(pattern, expr [, escape]):
        // the pattern is the FIRST argument searched.
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => explicit_collation(pattern)
            .or_else(|| explicit_collation(expr))
            .or_else(|| escape.as_deref().and_then(explicit_collation)),
        Expr::Is { left, right, .. } => {
            explicit_collation(left).or_else(|| explicit_collation(right))
        }
        Expr::IsNull { expr, .. } => explicit_collation(expr),
        _ => None,
    }
}

/// Resolve a collation NAME for a comparison: BINARY (the default) is
/// `None` — the connection's ordinary order; an unknown name errors.
pub(crate) fn resolve_collation(
    name: Option<String>,
) -> Result<Option<std::sync::Arc<dyn crate::plugin::Collation>>> {
    match name {
        None => Ok(None),
        Some(n) if n.eq_ignore_ascii_case("binary") => Ok(None),
        Some(n) => crate::plugin::lookup_collation(&n)
            .map(Some)
            .ok_or_else(|| {
                crate::error::Error::semantic(format!("no such collation sequence: {}", n))
            }),
    }
}

/// Apply a comparison affinity to an operand's VALUE (SQLite's
/// sqlite3VdbeMemApplyAffinity for comparisons): TEXT affinity renders
/// numbers as text; the numeric affinities convert numeric-LOOKING text
/// only (numbers pass through, other text keeps its storage class so the
/// comparison stays cross-type — SQLite's documented behavior); BLOB and
/// no-affinity never convert.
pub(crate) fn apply_operand_affinity(a: crate::types::Affinity, v: &Value) -> Value {
    use crate::types::Affinity::*;
    match a {
        Text => match v {
            Value::Integer(_) | Value::Real(_) => Value::Text(v.as_text().into()),
            _ => v.clone(),
        },
        // Comparison affinity (datatype3 §4.2): an INTEGER, REAL or
        // NUMERIC column lends NUMERIC affinity to the other operand —
        // NOT its own storage affinity. The storage INTEGER coerce
        // truncates REALs (`Real(5e-324)` -> `Integer(0)`), which flipped
        // `a <> 5e-324` on a=0 from TRUE to FALSE (stateful-fuzz
        // divergence). NUMERIC keeps non-integral REALs as REALs and
        // only converts numeric-looking TEXT.
        Integer | Real | Numeric => {
            crate::storage::row_codec::affinity_apply_opt(Numeric, v.clone())
                .unwrap_or_else(|| v.clone())
        }
        _ => v.clone(),
    }
}

/// `sqlite3ExprAffinity`: the affinity an expression carries into a
/// comparison — a column's declared affinity, a CAST's target affinity,
/// and the operand affinity seen through COLLATE. Every other expression (literals, parameters,
/// arithmetic, `||`, most function calls) has NO affinity (`None`).
/// `col_aff` resolves column references in the caller's context.
pub(crate) fn expr_affinity_with(
    e: &Expr,
    col_aff: &dyn Fn(&Expr) -> Option<crate::types::Affinity>,
) -> Option<crate::types::Affinity> {
    match e {
        Expr::Column { .. } => col_aff(e),
        Expr::Cast { type_name, expr } => {
            if let Some(a) = marker_affinity(type_name, AFF_MARKER) {
                Some(a)
            } else if type_name.starts_with(IN_RHS_MARKER) || type_name.starts_with(SUBQ_MARKER) {
                expr_affinity_with(expr, col_aff)
            } else {
                Some(crate::types::Affinity::from_declared_type(type_name))
            }
        }
        Expr::Collate { expr, .. } => expr_affinity_with(expr, col_aff),
        // (likely()/unlikely() do NOT pass their argument's affinity
        // through in 3.53 — `likely(intcol) = '1'` compares raw.)
        _ => None,
    }
}

/// The affinity a comparison applies (`sqlite3CompareAffinity`):
/// both sides carry an affinity → NUMERIC if either is numeric, else
/// none; exactly one side carries an affinity → that one (BLOB = none);
/// neither → none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CmpAffinity {
    None,
    Numeric,
    Text,
}

pub(crate) fn comparison_affinity(
    la: Option<crate::types::Affinity>,
    ra: Option<crate::types::Affinity>,
) -> CmpAffinity {
    use crate::types::Affinity as A;
    let numeric = |x: A| matches!(x, A::Integer | A::Real | A::Numeric);
    match (la, ra) {
        (Some(a), Some(b)) => {
            if numeric(a) || numeric(b) {
                CmpAffinity::Numeric
            } else {
                CmpAffinity::None
            }
        }
        (Some(a), None) | (None, Some(a)) => {
            if numeric(a) {
                CmpAffinity::Numeric
            } else if a == A::Text {
                CmpAffinity::Text
            } else {
                CmpAffinity::None
            }
        }
        (None, None) => CmpAffinity::None,
    }
}

/// Apply a comparison affinity to the operand VALUES exactly like the
/// OP_Eq/OP_Lt family: NUMERIC converts each pure TEXT operand that
/// looks like a number (`applyNumericAffinity(p, 0)`) when at least one
/// operand is TEXT; TEXT renders each numeric operand as text when at
/// least one operand is TEXT. BLOBs and NULLs never convert.
pub(crate) fn apply_comparison_affinity(aff: CmpAffinity, l: Value, r: Value) -> (Value, Value) {
    let any_text = matches!(l, Value::Text(_)) || matches!(r, Value::Text(_));
    if !any_text {
        return (l, r);
    }
    match aff {
        CmpAffinity::None => (l, r),
        CmpAffinity::Numeric => {
            let conv = |v: Value| -> Value {
                if matches!(v, Value::Text(_)) {
                    numeric_value(&v).unwrap_or(v)
                } else {
                    v
                }
            };
            (conv(l), conv(r))
        }
        CmpAffinity::Text => {
            let conv = |v: Value| -> Value {
                if matches!(v, Value::Integer(_) | Value::Real(_)) {
                    Value::Text(v.as_text().into())
                } else {
                    v
                }
            };
            (conv(l), conv(r))
        }
    }
}

/// SQLite's comparison-affinity contract for one operator application
/// (datatype3 §4.2) in an evaluation context: the affinities of both
/// operand EXPRESSIONS decide the comparison affinity, which is then
/// applied to the operand VALUES. Columns the context cannot resolve to
/// an affinity count as "no affinity" (the conservative raw compare).
fn coerce_comparison_operands(
    ctx: &EvalContext<'_>,
    le: &Expr,
    l: Value,
    re: &Expr,
    r: Value,
) -> (Value, Value) {
    // Affinity only ever changes a comparison when a TEXT operand is
    // involved (OP_Eq's conversions are gated on MEM_Str): skip the
    // static resolution entirely for the numeric/NULL/BLOB common case.
    if !matches!(l, Value::Text(_)) && !matches!(r, Value::Text(_)) {
        return (l, r);
    }
    let col = |e: &Expr| ctx.comparison_column_affinity(e);
    let aff = comparison_affinity(expr_affinity_with(le, &col), expr_affinity_with(re, &col));
    apply_comparison_affinity(aff, l, r)
}

/// Evaluate an expression in the given context.
/// sqlite3ExprSimplifiedAndOr: an AND / OR with an ALWAYS-true or
/// ALWAYS-false operand (an integer literal — SQLite's EP_IsTrue /
/// EP_IsFalse; TRUE / FALSE parse to one) reduces, recursively, to the
/// side that decides the result: `X AND 0` → `0`, `X OR 1` → `1`,
/// `X AND 1` / `X OR 0` → `X`. Returns `e` itself when nothing reduces.
fn simplified_and_or(e: &Expr) -> &Expr {
    if let Expr::Binary {
        op: op @ (BinaryOp::And | BinaryOp::Or),
        left,
        right,
    } = e
    {
        let r = simplified_and_or(right);
        let l = simplified_and_or(left);
        // EP_IsTrue / EP_IsFalse: a 32-bit integer literal only.
        let always = |x: &Expr, truth: bool| {
            crate::planner::fold::is_truth_literal(x)
                && matches!(x, Expr::Literal(Value::Integer(n)) if (*n != 0) == truth)
        };
        if always(l, true) || always(r, false) {
            return if *op == BinaryOp::And { r } else { l };
        }
        if always(r, true) || always(l, false) {
            return if *op == BinaryOp::And { l } else { r };
        }
    }
    e
}

/// EP_Subquery: does the expression tree hold a subquery anywhere? A
/// non-allocating walk (the evaluator asks per operand, per row).
pub(crate) fn contains_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::Exists(_) => true,
        Expr::In { expr, source, .. } => {
            matches!(source, InSource::Subquery(_))
                || contains_subquery(expr)
                || matches!(source, InSource::List(l) if l.iter().any(contains_subquery))
        }
        Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
            contains_subquery(left) || contains_subquery(right)
        }
        // A materialized subquery's carrier (see SUBQ_MARKER).
        Expr::Cast { type_name, .. }
            if type_name.starts_with(SUBQ_MARKER)
                || type_name.starts_with(AFF_MARKER)
                || type_name.starts_with(IN_RHS_MARKER) =>
        {
            true
        }
        Expr::Unary { expr, .. }
        | Expr::IsNull { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. } => contains_subquery(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_subquery(expr) || contains_subquery(low) || contains_subquery(high),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_subquery(expr)
                || contains_subquery(pattern)
                || escape.as_deref().is_some_and(contains_subquery)
        }
        Expr::Row(items) => items.iter().any(contains_subquery),
        Expr::Function { args, filter, .. } => {
            args.iter().any(contains_subquery) || filter.as_deref().is_some_and(contains_subquery)
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(contains_subquery)
                || whens
                    .iter()
                    .any(|(w, t)| contains_subquery(w) || contains_subquery(t))
                || else_.as_deref().is_some_and(contains_subquery)
        }
        Expr::Raise { message, .. } => message.as_deref().is_some_and(contains_subquery),
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Column { .. } => false,
    }
}

/// sqlite3ExprCanBeNull, conservatively: only a non-NULL literal is known
/// never to be NULL.
fn can_be_null(e: &Expr) -> bool {
    !matches!(e, Expr::Literal(v) if !v.is_null())
}

/// SQLite's exprEvalRhsFirst: with a subquery only on the LEFT, the
/// right operand of an AND / OR goes first.
fn and_or_order<'a>(left: &'a Expr, right: &'a Expr) -> (&'a Expr, &'a Expr) {
    if contains_subquery(left) && !contains_subquery(right) {
        (right, left)
    } else {
        (left, right)
    }
}

/// A condition — a WHERE / ON / HAVING term, an aggregate or window
/// FILTER, a searched CASE's WHEN, a trigger WHEN, a partial-index WHERE,
/// an upsert's WHERE: TRUE passes, FALSE and NULL do not
/// (`sqlite3ExprIfFalse(…, SQLITE_JUMPIFNULL)`). AND / OR / NOT /
/// `IS [NOT] TRUE|FALSE` / BETWEEN short-circuit here the way SQLite's
/// jump code does; a value-context AND / OR (`evaluate`) computes both
/// operands instead.
pub fn evaluate_where(expr: &Expr, ctx: &EvalContext<'_>) -> Result<bool> {
    Ok(!cond_if_false(expr, ctx, true)?)
}

/// A CHECK constraint: it fails only on FALSE — NULL passes
/// (`sqlite3ExprIfTrue(…, SQLITE_JUMPIFNULL)` to the all-ok label).
pub fn evaluate_check(expr: &Expr, ctx: &EvalContext<'_>) -> Result<bool> {
    cond_if_true(expr, ctx, true)
}

/// sqlite3ExprIfFalse: does `e` take its FALSE exit — FALSE, or NULL when
/// `jump_if_null`? Mirrors the jump code's evaluation order exactly: the
/// operands a decided branch skips are never evaluated (so they cannot
/// raise).
fn cond_if_false(e: &Expr, ctx: &EvalContext<'_>, jump_if_null: bool) -> Result<bool> {
    match e {
        Expr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            left,
            right,
        } => {
            let alt = simplified_and_or(e);
            if !std::ptr::eq(alt, e) {
                return cond_if_false(alt, ctx, jump_if_null);
            }
            let (first, second) = and_or_order(left, right);
            if *op == BinaryOp::And {
                Ok(cond_if_false(first, ctx, jump_if_null)?
                    || cond_if_false(second, ctx, jump_if_null)?)
            } else {
                if cond_if_true(first, ctx, !jump_if_null)? {
                    return Ok(false);
                }
                cond_if_false(second, ctx, jump_if_null)
            }
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => cond_if_true(expr, ctx, jump_if_null),
        // TK_TRUTH never yields NULL: the operand's own NULL decides by
        // the form (IS TRUE: NULL fails it; IS NOT FALSE: NULL passes).
        Expr::Unary {
            op: UnaryOp::Truth { truth, negated },
            expr,
        } => {
            if *truth ^ *negated {
                cond_if_false(expr, ctx, !*negated)
            } else {
                cond_if_true(expr, ctx, !*negated)
            }
        }
        Expr::Between { negated: true, .. } => between_jump(e, ctx, true, jump_if_null),
        Expr::Between { .. } => between_jump(e, ctx, false, jump_if_null),
        _ => {
            let v = evaluate(e, ctx)?;
            Ok(if v.is_null() {
                jump_if_null
            } else {
                !v.is_truthy()
            })
        }
    }
}

/// sqlite3ExprIfTrue: does `e` take its TRUE exit — TRUE, or NULL when
/// `jump_if_null`?
fn cond_if_true(e: &Expr, ctx: &EvalContext<'_>, jump_if_null: bool) -> Result<bool> {
    match e {
        Expr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            left,
            right,
        } => {
            let alt = simplified_and_or(e);
            if !std::ptr::eq(alt, e) {
                return cond_if_true(alt, ctx, jump_if_null);
            }
            let (first, second) = and_or_order(left, right);
            if *op == BinaryOp::And {
                if cond_if_false(first, ctx, !jump_if_null)? {
                    return Ok(false);
                }
                cond_if_true(second, ctx, jump_if_null)
            } else {
                Ok(cond_if_true(first, ctx, jump_if_null)?
                    || cond_if_true(second, ctx, jump_if_null)?)
            }
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => cond_if_false(expr, ctx, jump_if_null),
        Expr::Unary {
            op: UnaryOp::Truth { truth, negated },
            expr,
        } => {
            if *truth ^ *negated {
                cond_if_true(expr, ctx, *negated)
            } else {
                cond_if_false(expr, ctx, *negated)
            }
        }
        // NOT BETWEEN parses as TK_NOT over TK_BETWEEN.
        Expr::Between { negated: true, .. } => between_jump(e, ctx, false, jump_if_null),
        Expr::Between { .. } => between_jump(e, ctx, true, jump_if_null),
        _ => {
            let v = evaluate(e, ctx)?;
            Ok(if v.is_null() {
                jump_if_null
            } else {
                v.is_truthy()
            })
        }
    }
}

/// exprCodeBetween under a jump: `x BETWEEN lo AND hi` is `x >= lo AND
/// x <= hi` with x computed once, the AND coded as a jump — the upper
/// bound is never evaluated once `x >= lo` decides. `if_true` selects
/// sqlite3ExprIfTrue (else IfFalse) over the un-negated BETWEEN.
fn between_jump(
    e: &Expr,
    ctx: &EvalContext<'_>,
    if_true: bool,
    jump_if_null: bool,
) -> Result<bool> {
    let Expr::Between {
        expr, low, high, ..
    } = e
    else {
        unreachable!("between_jump on a non-BETWEEN");
    };
    if is_row(expr) || is_row(low) || is_row(high) {
        // Row values: the same jump over the two row comparisons.
        let truth = |v: Value| -> Option<bool> {
            if v.is_null() {
                None
            } else {
                Some(v.is_truthy())
            }
        };
        let ge = truth(row_binary(BinaryOp::GtEq, expr, low, ctx, false)?);
        let first_jump_null = if if_true { !jump_if_null } else { jump_if_null };
        if ge.map_or(first_jump_null, |b| !b) {
            return Ok(!if_true);
        }
        let le = truth(row_binary(BinaryOp::LtEq, expr, high, ctx, false)?);
        return Ok(le.map_or(jump_if_null, |b| b == if_true));
    }
    let v = evaluate(expr, ctx)?;
    let ge = between_bound(ctx, expr, &v, low, true)?;
    // The AND's first comparison, under the jump flags of its position.
    let first_jump_null = if if_true { !jump_if_null } else { jump_if_null };
    let first_false = match ge {
        None => first_jump_null,
        Some(b) => !b,
    };
    if first_false {
        // IfFalse: the AND is decided FALSE — take the jump. IfTrue: the
        // AND's first operand jumped to the fall-through label.
        return Ok(!if_true);
    }
    let le = between_bound(ctx, expr, &v, high, false)?;
    Ok(match le {
        None => jump_if_null,
        Some(b) => b == if_true,
    })
}

/// One desugared BETWEEN comparison — `v >= bound` (`lower`) or `v <=
/// bound` — with that pair's comparison affinity and collation; `None`
/// when either side is NULL.
fn between_bound(
    ctx: &EvalContext<'_>,
    expr: &Expr,
    v: &Value,
    bound: &Expr,
    lower: bool,
) -> Result<Option<bool>> {
    let b = evaluate(bound, ctx)?;
    if v.is_null() || b.is_null() {
        return Ok(None);
    }
    let (v, b) = coerce_comparison_operands(ctx, expr, v.clone(), bound, b);
    let coll = resolve_collation(ctx.binary_comparison_collation(expr, bound))?;
    let ord = match coll {
        Some(c) => crate::plugin::compare_collated(&v, &b, c.as_ref()),
        None => crate::executor::value_cmp_conn(&v, &b),
    };
    Ok(Some(if lower {
        ord != std::cmp::Ordering::Less
    } else {
        ord != std::cmp::Ordering::Greater
    }))
}

/// SQLite's error for a row value outside a row context, or rows of
/// different sizes.
fn row_value_misused() -> Error {
    Error::semantic("row value misused")
}

fn is_row(e: &Expr) -> bool {
    matches!(e, Expr::Row(_))
}

/// One side of a row-value comparison: a `(a, b, …)` row whose items are
/// evaluated as the comparison reaches them (an earlier deciding pair
/// skips the rest, as in codeVectorCompare), or already-materialized
/// values (a correlated subquery's row, an IN operand computed once).
enum RowSide<'e> {
    Items(&'e [Expr]),
    Values(Option<&'e [Expr]>, Vec<Value>),
}

impl RowSide<'_> {
    fn len(&self) -> usize {
        match self {
            RowSide::Items(items) => items.len(),
            RowSide::Values(_, vals) => vals.len(),
        }
    }

    /// Item `i`: its expression (for comparison affinity / collation)
    /// and value.
    fn item(&self, i: usize, ctx: &EvalContext<'_>) -> Result<(Option<&Expr>, Value)> {
        match self {
            RowSide::Items(items) => Ok((Some(&items[i]), evaluate(&items[i], ctx)?)),
            RowSide::Values(exprs, vals) => Ok((exprs.map(|e| &e[i]), vals[i].clone())),
        }
    }
}

/// The row side of `e`, sized against the other side's `width`: a row
/// constructor, or a (correlated) subquery's first row — all NULL when it
/// returns no row. `None` = not a row-shaped operand.
fn row_side<'e>(e: &'e Expr, width: usize, ctx: &EvalContext<'_>) -> Result<Option<RowSide<'e>>> {
    match e {
        Expr::Row(items) => Ok(Some(RowSide::Items(items))),
        Expr::Subquery(sel) => {
            let res = crate::executor::corr_exec_rows(sel, ctx, true)?;
            if res.columns.len() != width {
                return Err(Error::semantic(format!(
                    "sub-select returns {} columns - expected {}",
                    res.columns.len(),
                    width
                )));
            }
            let row = res
                .rows
                .into_iter()
                .next()
                .unwrap_or_else(|| vec![Value::Null; width]);
            Ok(Some(RowSide::Values(None, row)))
        }
        _ => Ok(None),
    }
}

/// One pair of a row-value comparison under the scalar rules: the pair's
/// comparison affinity and collation, then SQLite's value order. `None`
/// when either side is NULL — unless `null_eq` (IS / IS NOT), where
/// NULL equals only NULL.
fn row_pair_cmp(
    ctx: &EvalContext<'_>,
    le: Option<&Expr>,
    l: Value,
    re: Option<&Expr>,
    r: Value,
    null_eq: bool,
) -> Result<Option<std::cmp::Ordering>> {
    use std::cmp::Ordering;
    if l.is_null() || r.is_null() {
        if null_eq {
            return Ok(Some(if l.is_null() && r.is_null() {
                Ordering::Equal
            } else {
                Ordering::Less
            }));
        }
        return Ok(None);
    }
    let none = Expr::Literal(Value::Null);
    let (le, re) = (le.unwrap_or(&none), re.unwrap_or(&none));
    let (l, r) = coerce_comparison_operands(ctx, le, l, re, r);
    let coll = resolve_collation(ctx.binary_comparison_collation(le, re))?;
    Ok(Some(match coll {
        Some(c) => crate::plugin::compare_collated(&l, &r, c.as_ref()),
        None => crate::executor::value_cmp_conn(&l, &r),
    }))
}

/// A row-value comparison (codeVectorCompare): `=` is the Kleene AND of
/// the pairs' equalities, stopping at the first definite FALSE; `<>` its
/// negation; `<` `<=` `>` `>=` compare lexicographically — an earlier
/// pair decides when it is unequal, a NULL there makes the whole result
/// NULL, the last pair applies the operator itself. `null_eq` is IS /
/// IS NOT (NULL equals NULL; never NULL).
fn row_compare(
    op: BinaryOp,
    l: &RowSide<'_>,
    r: &RowSide<'_>,
    ctx: &EvalContext<'_>,
    null_eq: bool,
) -> Result<Value> {
    use std::cmp::Ordering;
    if l.len() != r.len() {
        return Err(row_value_misused());
    }
    let eq_mode = matches!(op, BinaryOp::Eq | BinaryOp::NotEq);
    if !eq_mode
        && !matches!(
            op,
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
        )
    {
        return Err(row_value_misused());
    }
    let n = l.len();
    let mut result = Some(true);
    for i in 0..n {
        let last = i + 1 == n;
        let (le, lv) = l.item(i, ctx)?;
        let (re, rv) = r.item(i, ctx)?;
        let ord = row_pair_cmp(ctx, le, lv, re, rv, null_eq)?;
        if eq_mode {
            match ord {
                Some(Ordering::Equal) => {}
                Some(_) => {
                    result = Some(false);
                    break;
                }
                None => result = None,
            }
            continue;
        }
        let Some(o) = ord else {
            result = None;
            break;
        };
        // Earlier pairs use the STRICT operator; ties move on.
        let pass = match (op, last) {
            (BinaryOp::Lt, _) | (BinaryOp::LtEq, false) => o == Ordering::Less,
            (BinaryOp::LtEq, true) => o != Ordering::Greater,
            (BinaryOp::Gt, _) | (BinaryOp::GtEq, false) => o == Ordering::Greater,
            _ => o != Ordering::Less,
        };
        if pass {
            break;
        }
        if !last && o == Ordering::Equal {
            continue;
        }
        result = Some(false);
        break;
    }
    let result = if op == BinaryOp::NotEq {
        result.map(|b| !b)
    } else {
        result
    };
    Ok(match result {
        Some(b) => Value::Integer(i64::from(b)),
        None => Value::Null,
    })
}

/// `left op right` where a side is a row value.
fn row_binary(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    ctx: &EvalContext<'_>,
    null_eq: bool,
) -> Result<Value> {
    let width = match (left, right) {
        (Expr::Row(items), _) | (_, Expr::Row(items)) => items.len(),
        _ => return Err(row_value_misused()),
    };
    let (Some(l), Some(r)) = (row_side(left, width, ctx)?, row_side(right, width, ctx)?) else {
        return Err(row_value_misused());
    };
    row_compare(op, &l, &r, ctx, null_eq)
}

/// `(a, b, …) [NOT] IN (…)`: the Kleene OR of the row's equality with
/// each right-hand row — a `((…), (…))` list of rows, or a subquery's
/// rows (uncorrelated ones arrive materialized as a list); an empty right
/// side is FALSE. The left row is computed once.
fn evaluate_in_row(
    items: &[Expr],
    source: &InSource,
    negated: bool,
    ctx: &EvalContext<'_>,
) -> Result<Value> {
    let width = items.len();
    let lhs_vals = items
        .iter()
        .map(|e| evaluate(e, ctx))
        .collect::<Result<Vec<_>>>()?;
    let lhs = RowSide::Values(Some(items), lhs_vals);
    let mut saw_null = false;
    let mut any = false;
    let mut probe = |rhs: RowSide<'_>| -> Result<bool> {
        any = true;
        match row_compare(BinaryOp::Eq, &lhs, &rhs, ctx, false)? {
            Value::Integer(1) => Ok(true),
            Value::Null => {
                saw_null = true;
                Ok(false)
            }
            _ => Ok(false),
        }
    };
    let found = match source {
        InSource::List(list) => {
            let mut found = false;
            for e in list.iter() {
                let Expr::Row(r) = e else {
                    return Err(row_value_misused());
                };
                if probe(RowSide::Items(r))? {
                    found = true;
                    break;
                }
            }
            found
        }
        InSource::Subquery(sel) => {
            let res = crate::executor::corr_exec_rows(sel, ctx, false)?;
            if res.columns.len() != width {
                return Err(Error::semantic(format!(
                    "sub-select returns {} columns - expected {}",
                    res.columns.len(),
                    width
                )));
            }
            let mut found = false;
            for row in res.rows {
                if probe(RowSide::Values(None, row))? {
                    found = true;
                    break;
                }
            }
            found
        }
        InSource::Table(_) => return Err(row_value_misused()),
    };
    Ok(if found {
        Value::Integer(i64::from(!negated))
    } else if !any {
        Value::Integer(i64::from(negated))
    } else if saw_null {
        Value::Null
    } else {
        Value::Integer(i64::from(negated))
    })
}

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
        // A VALUE-context AND / OR (a result column, an operand, an
        // argument, a SET value) follows exprCodeTargetAndOr: BOTH
        // operands are computed — `SELECT x > 0 OR abs(y)` raises on an
        // overflowing y even when x > 0 — except that an operand holding
        // a subquery is skipped when the other one decides (and goes
        // second: exprEvalRhsFirst). Conditions short-circuit instead
        // (`evaluate_where`).
        Expr::Binary {
            op: op @ (BinaryOp::And | BinaryOp::Or),
            left,
            right,
        } => {
            // sqlite3ExprSimplifiedAndOr (every context): an operand that
            // is ALWAYS false/true — an integer literal — decides, and the
            // other side is never evaluated (`abs(-9223372036854775808)
            // AND 0` is 0); the survivor's truth value is the result.
            let alt = simplified_and_or(expr);
            if !std::ptr::eq(alt, expr) {
                let v = evaluate(alt, ctx)?;
                return Ok(apply_binary(BinaryOp::And, &v, &v));
            }
            let (first, second) = and_or_order(left, right);
            let a = evaluate(first, ctx)?;
            if !a.is_null() && contains_subquery(second) {
                let t = a.is_truthy();
                if *op == BinaryOp::And && !t {
                    return Ok(Value::Integer(0));
                }
                if *op == BinaryOp::Or && t {
                    return Ok(Value::Integer(1));
                }
            }
            let b = evaluate(second, ctx)?;
            Ok(apply_binary(*op, &a, &b))
        }
        Expr::Binary { op, left, right } if is_row(left) || is_row(right) => {
            row_binary(*op, left, right, ctx, false)
        }
        Expr::Binary { op, left, right } => {
            // exprComputeOperands (comparisons, arithmetic, bitwise, shifts,
            // `||` — every operator that is NULL when an operand is NULL):
            // an operand holding a subquery goes second and is SKIPPED
            // when the other one is NULL — `NULL + CASE WHEN
            // abs(-9223372036854775808) THEN (SELECT 1) END` is NULL, not
            // "integer overflow". With the subquery only on the LEFT and a
            // right operand that may be NULL, the right one goes first.
            let null_skips = matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
                    | BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::ShiftLeft
                    | BinaryOp::ShiftRight
                    | BinaryOp::Concat
            );
            let (l, r) = if null_skips
                && can_be_null(right)
                && contains_subquery(left)
                && !contains_subquery(right)
            {
                let r = evaluate(right, ctx)?;
                if r.is_null() {
                    return Ok(Value::Null);
                }
                (evaluate(left, ctx)?, r)
            } else {
                let l = evaluate(left, ctx)?;
                if null_skips && l.is_null() && contains_subquery(right) {
                    return Ok(Value::Null);
                }
                (l, evaluate(right, ctx)?)
            };
            // Comparison-affinity coercion (SQLite's exprAffinity rules):
            // applied BEFORE the collation dispatch — `text_col > 0` must
            // compare TEXT-to-TEXT even under a COLLATE, and
            // `num_col COLLATE X > '5'` must still convert '5' first.
            let (l, r) = if matches!(
                op,
                BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
            ) {
                coerce_comparison_operands(ctx, left, l, right, r)
            } else {
                (l, r)
            };
            // JSON path operators: `x -> path` (JSON text) / `x ->> path`
            // (SQL value). They bind tighter than every other binary
            // operator and can RAISE (malformed JSON / bad path).
            if matches!(op, BinaryOp::Arrow | BinaryOp::ArrowText) {
                return arrow_eval(*op, &l, &r);
            }
            // PostgreSQL-borrowed operators, also RAISE-capable (they parse
            // their operands as tsvector/tsquery / WKT geometry):
            //   `tsvector @@ tsquery` — full-text match
            //   `geom <-> geom`       — PostGIS-style planar KNN distance
            if matches!(op, BinaryOp::FtsMatch) {
                return crate::executor::fts::eval_match_op(&l, &r);
            }
            if matches!(op, BinaryOp::Distance) {
                return crate::executor::geo::eval_distance_op(&l, &r);
            }
            // COLLATE on either comparison operand applies a collation to
            // text comparison (SQLite: `a < b COLLATE NOCASE`). Only
            // ordering / equality operators honor it.
            if let Some(coll_name) = ctx.binary_comparison_collation(left, right) {
                // BINARY is the DEFAULT collation (SQLite accepts
                // `COLLATE BINARY` everywhere) — plain apply_binary, no
                // lookup (lookup_collation maps "binary" to None).
                if coll_name.eq_ignore_ascii_case("binary") {
                    return Ok(apply_binary(*op, &l, &r));
                }
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
        } if is_row(expr) || is_row(low) || is_row(high) => {
            // `x >= lo AND x <= hi` over rows, both computed (value
            // context), Kleene AND.
            let ge = row_binary(BinaryOp::GtEq, expr, low, ctx, false)?;
            let le = row_binary(BinaryOp::LtEq, expr, high, ctx, false)?;
            let v = apply_binary(BinaryOp::And, &ge, &le);
            Ok(if *negated {
                apply_unary(UnaryOp::Not, &v)
            } else {
                v
            })
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
            // Comparison affinity (SQLite: BETWEEN desugars to `x >= lo
            // AND x <= hi`, each comparison applying the affinity of its
            // own operand pair). The value side is coerced per pair.
            let (v_lo, lo) = coerce_comparison_operands(ctx, expr, v.clone(), low, lo);
            let (v_hi, hi) = coerce_comparison_operands(ctx, expr, v.clone(), high, hi);
            // SQL three-valued logic: BETWEEN is sugar for `expr >= low
            // AND expr <= high`, and that AND is KLEENE — FALSE dominates
            // NULL. Bailing to NULL on ANY null operand is WRONG for the
            // NOT form: `93 NOT BETWEEN NULL AND 36` is NOT(NULL AND
            // FALSE) = NOT FALSE = TRUE in SQLite (pinned by the
            // canonical sqllogictest corpus: the old early-bail returned
            // NULL and dropped rows SQLite keeps —
            // `SELECT 79 ... WHERE 93 NOT BETWEEN NULL AND col1+33`).
            // Compute both comparisons NULL-aware, then Kleene-AND:
            //   FALSE AND anything     = FALSE
            //   NULL  AND TRUE|NULL    = NULL
            //   TRUE  AND TRUE         = TRUE
            let ge_null = v.is_null() || lo.is_null();
            let le_null = v.is_null() || hi.is_null();
            // A COLLATE on the value operand applies to both bound
            // comparisons (SQLite). Both sides evaluate as
            // Option<bool>: None = the comparison is NULL (unknown).
            // Each desugared comparison takes its own pair's collation
            // (sqlite3BinaryCompareCollSeq): the value operand's when it
            // has one, else the bound's.
            let coll_lo = resolve_collation(ctx.binary_comparison_collation(expr, low))?;
            let coll_hi = resolve_collation(ctx.binary_comparison_collation(expr, high))?;
            // Encoding-aware default: TEXT×TEXT pairs order under the
            // connection's file encoding (same rule as the binary
            // comparison operators — see apply_binary).
            let cmp =
                |a: &Value, b: &Value, c: &Option<std::sync::Arc<dyn crate::plugin::Collation>>| {
                    match c {
                        Some(c) => crate::plugin::compare_collated(a, b, c.as_ref()),
                        None => crate::executor::value_cmp_conn(a, b),
                    }
                };
            let ge = if ge_null {
                None
            } else {
                Some(cmp(&v_lo, &lo, &coll_lo) != std::cmp::Ordering::Less)
            };
            let le = if le_null {
                None
            } else {
                Some(cmp(&v_hi, &hi, &coll_hi) != std::cmp::Ordering::Greater)
            };
            // Kleene AND: FALSE dominates; NULL only when neither side
            // is FALSE and at least one is NULL.
            let in_range: Option<bool> = match (ge, le) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            match in_range {
                Some(b) => Ok(Value::Integer(if b ^ negated { 1 } else { 0 })),
                // UNKNOWN: BETWEEN and NOT BETWEEN are both NULL — WHERE
                // filters the row either way (NOT NULL is still NULL).
                None => Ok(Value::Null),
            }
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
            if let Some(e) = &esc {
                if like_escape_char(e).is_none() {
                    return Err(Error::runtime(
                        "ESCAPE expression must be a single character",
                    ));
                }
            }
            let result = match op {
                LikeOp::Like => like_match(&v, &p, esc.as_ref(), false),
                LikeOp::Glob => glob_match(&v, &p),
                // REGEXP: POSIX ERE via the engine's own NFA (regex.rs),
                // leftmost-longest, linear-time. NULL handling is the
                // shared 3VL block above. `x REGEXP y` puts the pattern on
                // the RIGHT (SQLite's regexp(y, x) convention).
                LikeOp::Regexp => {
                    // Compile through the thread-local cache: WHERE clauses
                    // evaluate this per row, and re-parsing per row is pure
                    // waste.
                    let re = crate::executor::regex::cached_compile(&p.as_text(), false)?;
                    re.find(&v.as_text()).is_some()
                }
                // MATCH stays SQLite-compat: no core match() function
                // exists (FTS5's hook); LIKE-shaped fallback.
                LikeOp::Match => like_match(&v, &p, esc.as_ref(), false),
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
        } if is_row(left) || is_row(right) => {
            let v = row_binary(BinaryOp::Eq, left, right, ctx, true)?;
            Ok(if *negated {
                apply_unary(UnaryOp::Not, &v)
            } else {
                v
            })
        }
        Expr::Is {
            left,
            right,
            negated,
        } => {
            let l = evaluate(left, ctx)?;
            let r = evaluate(right, ctx)?;
            // IS / IS NOT is OP_Eq / OP_Ne with SQLITE_NULLEQ: NULL equals
            // NULL, and otherwise the comparison applies the SAME
            // comparison affinity and collation as `=` (`realcol IS NOT
            // textcol` compares numerically when the text is numeric).
            let (l, r) = coerce_comparison_operands(ctx, left, l, right, r);
            let equal = if l.is_null() && r.is_null() {
                true
            } else if l.is_null() || r.is_null() {
                false
            } else {
                match ctx
                    .binary_comparison_collation(left, right)
                    .filter(|n| !n.eq_ignore_ascii_case("binary"))
                {
                    Some(name) => {
                        let coll = crate::plugin::lookup_collation(&name).ok_or_else(|| {
                            crate::error::Error::semantic(format!(
                                "no such collation sequence: {}",
                                name
                            ))
                        })?;
                        crate::plugin::compare_collated(&l, &r, coll.as_ref())
                            == std::cmp::Ordering::Equal
                    }
                    None => l == r,
                }
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
                    // `CASE x WHEN y` is SQLite's `x = y` (OP_Eq): NULL on
                    // either side never matches (`CASE NULL WHEN NULL` takes
                    // the ELSE branch).
                    let c = evaluate(cond, ctx)?;
                    let base = operand.as_deref().expect("operand present");
                    let (x, y) = coerce_comparison_operands(ctx, base, op.clone(), cond, c);
                    // ...with that comparison's collation too (the WHEN
                    // operand's declared one applies when the CASE operand
                    // is not a column: `CASE trim(x) WHEN nocase_col`).
                    let eq = match resolve_collation(ctx.binary_comparison_collation(base, cond))? {
                        Some(coll) => apply_binary_collated(BinaryOp::Eq, &x, &y, coll.as_ref()),
                        None => apply_binary(BinaryOp::Eq, &x, &y),
                    };
                    if eq.is_truthy() {
                        return evaluate(val, ctx);
                    }
                } else if evaluate_where(cond, ctx)? {
                    // The searched form's WHEN is a jump
                    // (sqlite3ExprIfFalse, SQLITE_JUMPIFNULL).
                    return evaluate(val, ctx);
                }
            }
            if let Some(e) = else_ {
                Ok(evaluate(e, ctx)?)
            } else {
                Ok(Value::Null)
            }
        }
        Expr::Row(_) => Err(row_value_misused()),
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
    if let Expr::Row(items) = expr {
        return evaluate_in_row(items, source, negated, ctx);
    }
    let v = evaluate(expr, ctx)?;
    // A COLLATE on the left operand applies to every membership
    // comparison (SQLite: `v COLLATE NOCASE IN ('a', 'B')`).
    // BINARY is the default collation: no lookup (lookup_collation maps
    // it to None, which must not read as "unknown collation").
    let coll_name = comparison_collation(expr)
        .or_else(|| ctx.expr_collation(expr))
        .filter(|n| !n.eq_ignore_ascii_case("binary"));
    let coll = coll_name
        .as_deref()
        .and_then(crate::plugin::lookup_collation);
    if let (Some(name), None) = (coll_name, &coll) {
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
    // An EMPTY right-hand side decides before the NULL rule: `x IN ()` /
    // `x IN (SELECT … no rows)` is FALSE (NOT IN: TRUE) even for a NULL
    // x — no element exists whose comparison could be UNKNOWN.
    if let InSource::List(list) = source {
        if list.is_empty() {
            return Ok(Value::Integer(i64::from(negated)));
        }
    }
    if v.is_null() {
        // A NULL left operand never matches, but SQLite still evaluates
        // the right-hand side: every list member (IN_INDEX_NOOP compares
        // member by member and only a MATCH stops it; an all-constant list
        // is materialized whole), and a subquery is materialized before
        // the left operand is even tested (sqlite3FindInIndex codes the
        // RHS first) — so a raising member raises:
        // `NULL IN (1, abs(-9223372036854775808))` is "integer overflow".
        match source {
            InSource::List(list) => {
                for e in list.iter() {
                    evaluate(e, ctx)?;
                }
            }
            InSource::Subquery(sel) => {
                // An EMPTY result decides first (see above): FALSE.
                if crate::executor::corr_exec_in_list(sel, ctx)?.is_empty() {
                    return Ok(Value::Integer(i64::from(negated)));
                }
            }
            InSource::Table(_) => {}
        }
        return Ok(Value::Null);
    }
    let coll_ref = coll.as_deref();
    // Comparison affinity (SQLite: the left operand's affinity applies to
    // every literal list member — `note IN ('x', -17.857)` compares
    // text-wise against '-17.857').
    // sqlite3ExprCodeIN with a list RHS: the comparison affinity is the
    // LEFT operand's alone (`comparisonAffinity` with no pRight), applied
    // OP_Eq-style to every member comparison — the members' own
    // affinities do not participate (`1 IN (textcol)` compares raw).
    let in_aff = match source {
        InSource::List(_) => {
            let col = |e: &Expr| ctx.comparison_column_affinity(e);
            let lhs_aff = expr_affinity_with(expr, &col);
            // A materialized `IN (SELECT …)` carries its column affinity
            // on the operand (see IN_RHS_MARKER): exprINAffinity combines
            // both sides like sqlite3CompareAffinity.
            match expr {
                Expr::Cast { type_name, .. } if type_name.starts_with(IN_RHS_MARKER) => {
                    comparison_affinity(lhs_aff, marker_affinity(type_name, IN_RHS_MARKER))
                }
                _ => comparison_affinity(lhs_aff, None),
            }
        }
        _ => CmpAffinity::None,
    };
    let (found, list_has_null) = match source {
        InSource::List(list) => {
            // sqlite3FindInIndex's two strategies: an all-CONSTANT list of
            // more than two members is materialized up front (every member
            // evaluates — a raising one raises even after a match), any
            // other list compares member by member and STOPS at the first
            // match (IN_INDEX_NOOP: `id IN (h, abs(-9223372036854775808))`
            // never evaluates abs() for a row whose id equals h). A match
            // decides the result alone: NULL members only matter when
            // nothing matched.
            let eager = list.len() > 2 && list.iter().all(is_constant_member);
            let pre: Vec<Value> = if eager {
                list.iter()
                    .map(|e| evaluate(e, ctx))
                    .collect::<Result<_>>()?
            } else {
                Vec::new()
            };
            let mut found = false;
            let mut list_has_null = false;
            for (i, e) in list.iter().enumerate() {
                let candidate = if eager {
                    pre[i].clone()
                } else {
                    evaluate(e, ctx)?
                };
                if candidate.is_null() {
                    list_has_null = true;
                    continue;
                }
                let (lv, candidate) = apply_comparison_affinity(in_aff, v.clone(), candidate);
                let eq = match coll_ref {
                    Some(c) => {
                        crate::plugin::compare_collated(&lv, &candidate, c)
                            == std::cmp::Ordering::Equal
                    }
                    None => lv == candidate,
                };
                if eq {
                    found = true;
                    break;
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

/// An IN-list member SQLite treats as constant (sqlite3ExprIsConstant):
/// no column or rowid reference, no subquery, no non-deterministic call.
fn is_constant_member(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) | Expr::Parameter(_) => true,
        Expr::Column { .. } | Expr::Subquery(_) | Expr::Exists(_) => false,
        Expr::Function {
            name, args, over, ..
        } => {
            over.is_none()
                && !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "random"
                        | "randomblob"
                        | "changes"
                        | "total_changes"
                        | "last_insert_rowid"
                        | "date"
                        | "time"
                        | "datetime"
                        | "julianday"
                        | "unixepoch"
                        | "strftime"
                )
                && args.iter().all(is_constant_member)
        }
        Expr::Binary { left, right, .. } | Expr::Is { left, right, .. } => {
            is_constant_member(left) && is_constant_member(right)
        }
        Expr::Unary { expr, .. }
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::IsNull { expr, .. } => is_constant_member(expr),
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().map_or(true, is_constant_member)
                && whens
                    .iter()
                    .all(|(c, v)| is_constant_member(c) && is_constant_member(v))
                && else_.as_deref().map_or(true, is_constant_member)
        }
        Expr::Between {
            expr, low, high, ..
        } => is_constant_member(expr) && is_constant_member(low) && is_constant_member(high),
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            is_constant_member(expr)
                && is_constant_member(pattern)
                && escape.as_deref().map_or(true, is_constant_member)
        }
        Expr::In { expr, source, .. } => {
            is_constant_member(expr)
                && matches!(source, InSource::List(l) if l.iter().all(is_constant_member))
        }
        Expr::Row(items) => items.iter().all(is_constant_member),
        Expr::Raise { .. } => false,
    }
}

fn evaluate_function(name: &str, args: &[Expr], ctx: &EvalContext<'_>) -> Result<Value> {
    // The constant-WHERE marker evaluated as an ordinary predicate (a
    // path that filters row by row instead of deciding it up front): the
    // conjunction, term by term, stopping at the first FALSE/NULL.
    if name == crate::planner::CONST_WHERE_FN {
        for a in args {
            if !evaluate_where(a, ctx)? {
                return Ok(Value::Integer(0));
            }
        }
        return Ok(Value::Integer(1));
    }
    // `SET (a, b) = (SELECT …)` column pick over a CORRELATED subquery
    // (uncorrelated ones were materialized by the subquery rewrite).
    if name == crate::sql::parser::ROW_COL_FN {
        if let [Expr::Literal(Value::Integer(k)), Expr::Literal(Value::Integer(n)), sub] = args {
            let width = *n as usize;
            return match row_side(sub, width, ctx)? {
                Some(RowSide::Values(_, row)) => {
                    Ok(row.get(*k as usize).cloned().unwrap_or(Value::Null))
                }
                Some(RowSide::Items(items)) => evaluate(&items[*k as usize], ctx),
                None => Err(row_value_misused()),
            };
        }
        return Err(row_value_misused());
    }
    let fname = name.to_ascii_lowercase();
    // coalesce / ifnull / iif / if: SQLite codes these INLINE and lazily
    // (iif compiles as CASE) — arguments past the deciding one are never
    // evaluated, so they cannot raise: `iif(0, abs(-9223372036854775808),
    // 1)` is 1, `coalesce(1, abs(-9223372036854775808))` is 1.
    match fname.as_str() {
        "coalesce" | "ifnull" if args.len() >= 2 => {
            for a in args {
                let v = evaluate(a, ctx)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        "iif" | "if" if args.len() >= 2 => {
            let mut k = 0;
            while k + 1 < args.len() {
                if evaluate_where(&args[k], ctx)? {
                    return evaluate(&args[k + 1], ctx);
                }
                k += 2;
            }
            return if args.len() % 2 == 1 && args.len() >= 3 {
                evaluate(&args[args.len() - 1], ctx)
            } else {
                Ok(Value::Null)
            };
        }
        _ => {}
    }
    // Virtual-table aux functions (FTS5's bm25 / highlight / snippet):
    // the call resolves against the statement's driving vtab scan when
    // one is installed. The first argument is the table reference — it
    // evaluates to the row's rowid through the table-self hidden column.
    if matches!(fname.as_str(), "bm25" | "highlight" | "snippet") {
        if let Some(inst) = crate::executor::vtab_exec::aux_tls_get() {
            let supported = inst
                .with_table(|vt| Ok(vt.aux_functions().contains(&fname.as_str())))
                .unwrap_or(false);
            if supported {
                let argvals: Result<Vec<Value>> = args.iter().map(|e| evaluate(e, ctx)).collect();
                let argvals = argvals?;
                let rowid = match argvals.first() {
                    Some(Value::Integer(r)) => *r,
                    _ => {
                        return Err(Error::semantic(format!(
                            "unable to use function {}: first argument must be the table",
                            fname
                        )))
                    }
                };
                return inst.with_table(|vt| vt.eval_aux(&fname, rowid, &argvals[1..]));
            }
        }
    }
    // Scalar functions only here; aggregates are handled by the Aggregate operator.
    let mut argvals: Vec<Value> = Vec::with_capacity(args.len());
    for (i, e) in args.iter().enumerate() {
        // A json_extract() VALUE argument of a JSON constructor: its JSON
        // subtype depends on the node it selects.
        if crate::executor::json::is_json_extract_call(e)
            && crate::executor::json::takes_json_value(&fname, i)
        {
            if let Expr::Function { args: inner, .. } = e {
                let vals: Vec<Value> = inner
                    .iter()
                    .map(|x| evaluate(x, ctx))
                    .collect::<Result<_>>()?;
                argvals.push(crate::executor::json::json_extract_subtyped(&vals)?);
                continue;
            }
        }
        argvals.push(evaluate(e, ctx)?);
    }
    crate::executor::json::apply_json_subtypes(&fname, args, &mut argvals);
    // SQLITE_FUNC_NEEDCOLL builtins (scalar min / max, nullif) compare
    // through the collation of their FIRST argument that has one — an
    // explicit COLLATE or a column's declared collation.
    let needs_coll = match fname.as_str() {
        "min" | "max" => argvals.len() > 1,
        "nullif" => argvals.len() == 2,
        _ => false,
    };
    if needs_coll {
        if let Some(coll) =
            resolve_collation(args.iter().find_map(|a| ctx.needcoll_arg_collation(a)))?
        {
            let cmp = |a: &Value, b: &Value| crate::plugin::compare_collated(a, b, coll.as_ref());
            if fname == "nullif" {
                return Ok(
                    if cmp(&argvals[0], &argvals[1]) == std::cmp::Ordering::Equal {
                        Value::Null
                    } else {
                        argvals[0].clone()
                    },
                );
            }
            if argvals.iter().any(|v| v.is_null()) {
                return Ok(Value::Null);
            }
            // func.c minmaxFunc: `(cmp(best, v) ^ mask) >= 0` — min moves
            // to a later TIE, max keeps the earlier one.
            let mut best = 0usize;
            for i in 1..argvals.len() {
                let c = cmp(&argvals[best], &argvals[i]);
                let take = if fname == "min" {
                    c != std::cmp::Ordering::Less
                } else {
                    c == std::cmp::Ordering::Less
                };
                if take {
                    best = i;
                }
            }
            return Ok(argvals[best].clone());
        }
    }
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
            // func.c absFunc's default arm: REAL, TEXT and BLOB all go
            // through sqlite3_value_double and yield a REAL (`abs('-5')`
            // is 5.0, `abs('abc')` is 0.0); `abs(-0.0)` keeps -0.0
            // (`rVal<0` is false for negative zero).
            Some(other) => {
                let r = other.as_real();
                Value::Real(if r < 0.0 { -r } else { r })
            }
        },
        "length" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Integer(v.length()),
        },
        // octet_length(X) — SQLite 3.46+ / SQL standard: the number of
        // bytes in X's encoding. TEXT/BLOB are their byte lengths;
        // numerics use the byte length of their text representation;
        // NULL is NULL.
        "octet_length" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(Value::Text(t)) => Value::Integer(t.as_str().len() as i64),
            Some(Value::Blob(b)) => Value::Integer(b.len() as i64),
            Some(v) => Value::Integer(v.as_text().len() as i64),
        },
        // concat(X,...) — SQLite 3.44+: string concatenation of all
        // non-NULL arguments (NULLs are skipped, not neutral elements).
        "concat" => {
            let mut buf = String::new();
            for v in args {
                if !v.is_null() {
                    buf.push_str(&v.as_text());
                }
            }
            Value::Text(buf.into())
        }
        // concat_ws(SEP, X, ...) — SQLite 3.44+: NULL SEP yields NULL;
        // non-NULL arguments are joined by SEP (NULLs skipped).
        "concat_ws" => {
            let Some(sep) = args.first() else {
                return Ok(Value::Null);
            };
            if sep.is_null() {
                return Ok(Value::Null);
            }
            // The separator precedes every non-NULL item but the first —
            // an EMPTY item still counts (`concat_ws(',', '', 'a')` is
            // ",a"; keying on "buffer non-empty" dropped it).
            let sep = sep.as_text();
            let mut buf = String::new();
            let mut items = 0usize;
            for v in args.iter().skip(1) {
                if !v.is_null() {
                    if items > 0 {
                        buf.push_str(&sep);
                    }
                    items += 1;
                    buf.push_str(&v.as_text());
                }
            }
            Value::Text(buf.into())
        }
        // glob(PATTERN, X) — the GLOB operator as a function (argument
        // order matches SQLite: pattern first, then the string).
        "glob" => match (args.first(), args.get(1)) {
            (Some(p), Some(v)) if !p.is_null() && !v.is_null() => {
                Value::Integer(i64::from(glob_match(v, p)))
            }
            _ => Value::Null,
        },
        "sqlite_source_id" => Value::Text(
            // The engine's build identity (SQLite's format: a timestamp
            // plus a source hash).
            "2026-09-08 00:00:00 48a229ceaef4985c50990b14116b6d856af09850".into(),
        ),
        // func.c lowerFunc / upperFunc (no ICU): byte-wise
        // sqlite3Tolower / sqlite3Toupper — ASCII letters only; every
        // non-ASCII character passes through unchanged (`upper('é')` is
        // 'é', `upper('ß')` is 'ß').
        "lower" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_ascii_lowercase().into()),
        },
        "upper" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(v.as_text().to_ascii_uppercase().into()),
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
        // func.c replaceFunc's check ORDER: a NULL string or pattern is
        // NULL; an EMPTY pattern returns the string unchanged — before the
        // replacement is even looked at (`replace(x, '', NULL)` is x);
        // only then does a NULL replacement make the result NULL.
        "replace" => {
            if args.len() != 3 || args[0].is_null() || args[1].is_null() {
                Value::Null
            } else {
                let s = args[0].as_text();
                let from = args[1].as_text();
                // `zPattern[0]==0`: a pattern whose text STARTS with a NUL
                // byte (x'00...') reads as empty in C.
                if from.is_empty() || from.starts_with('\0') {
                    Value::Text(s.into())
                } else if args[2].is_null() {
                    Value::Null
                } else {
                    Value::Text(s.replace(&from, &args[2].as_text()).into())
                }
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
                    // func.c substrFunc: sqlite3_value_blob() of a
                    // zero-length BLOB is a NULL pointer, and the function
                    // returns early — substr(x'', ...) is NULL (an empty
                    // TEXT still yields '').
                    if b.is_empty() {
                        return Ok(Value::Null);
                    }
                    let (begin, end) = substr_range(
                        b.len() as i64,
                        args[1].as_integer(),
                        if args.len() == 3 {
                            args[2].as_integer()
                        } else {
                            1_000_000_000 /* SQLITE_LIMIT_LENGTH: func.c substrFunc's default length (a far-negative start drives it below zero: empty result) */
                        },
                    );
                    return Ok(Value::Blob(b[begin as usize..end as usize].to_vec()));
                }
                let s = args[0].as_text();
                // func.c substrFunc walks sqlite3_value_text() as a C
                // string: TEXT ends at its first NUL byte.
                let s = match s.find('\0') {
                    Some(nul) => &s[..nul],
                    None => &s[..],
                };
                let z = if args.len() == 3 {
                    args[2].as_integer()
                } else {
                    1_000_000_000 /* SQLITE_LIMIT_LENGTH: func.c substrFunc's default length (a far-negative start drives it below zero: empty result) */
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
        // iif(C1, V1 [, C2, V2 ...] [, ELSE]) / if(...) — SQLite 3.48+:
        // the first true condition's value, else the trailing ELSE
        // argument (odd count), else NULL.
        "iif" | "if" => {
            let mut k = 0;
            while k + 1 < args.len() {
                if args[k].is_truthy() {
                    return Ok(args[k + 1].clone());
                }
                k += 2;
            }
            if args.len() % 2 == 1 && args.len() >= 3 {
                args[args.len() - 1].clone()
            } else {
                Value::Null
            }
        }
        // func.c roundFunc, verbatim: a NULL digit count is NULL, the
        // count clamps to [0, 30]; |X| > 2^52 has no fraction to round;
        // N == 0 rounds half away from zero through an i64; otherwise
        // the value is printed with "%!.*f" and parsed back.
        "round" => {
            let mut n: i64 = 0;
            if let Some(d) = args.get(1) {
                if d.is_null() {
                    return Ok(Value::Null);
                }
                n = d.as_integer().clamp(0, 30);
            }
            match args.first() {
                None | Some(Value::Null) => Value::Null,
                Some(v) => {
                    let mut r = v.as_real();
                    if (-4503599627370496.0..=4503599627370496.0).contains(&r) {
                        if n == 0 {
                            r = ((r + if r < 0.0 { -0.5 } else { 0.5 }) as i64) as f64;
                        } else {
                            let txt = crate::executor::printf::sql_printf(
                                b"%!.*f",
                                &[Value::Integer(n), Value::Real(r)],
                            );
                            r = crate::types::numeric::atof(txt.as_text().as_bytes()).0;
                        }
                    }
                    real_result(r)
                }
            }
        }
        "random" => Value::Integer(rand_i64()),
        // randomblob(N): N < 1 yields one byte (func.c).
        "randomblob" => {
            let n = args.first().map(|v| v.as_integer()).unwrap_or(0).max(1);
            if n > SQLITE_MAX_LENGTH {
                return Err(Error::runtime("string or blob too big"));
            }
            let mut out = vec![0u8; n as usize];
            fill_random(&mut out);
            Value::Blob(out)
        }
        // __JSON_EXTRACT_SUBTYPED(X, P...) — hidden scalar: json_extract
        // with SQLite's JSON subtype applied (see json_extract_subtyped);
        // feeds json_group_array.
        "__json_extract_subtyped" => crate::executor::json::json_extract_subtyped(args)?,
        // __JSON_OBJECT_FRAG(k, v) — hidden scalar feeding
        // json_group_object's accumulator: `"k":v` (JSON-quoted).
        // NULL values are INCLUDED as null (SQLite); NULL keys skip the
        // pair entirely.
        "__json_object_frag" => {
            if args.len() == 2 && !args[0].is_null() {
                let k = crate::executor::json::json_quote_value(&args[0]);
                let v = crate::executor::json::json_quote_value(&args[1]);
                Value::Text(format!("{k}:{v}").into())
            } else {
                Value::Null
            }
        }
        // __JSONB_OBJECT_FRAG(k, v) — the JSONB analogue: the encoded
        // key element + value element bytes, feeding jsonb_group_object.
        "__jsonb_object_frag" => {
            if args.len() == 2 && !args[0].is_null() {
                let key = crate::executor::jsonb::value_to_jb(&args[0]);
                let val = crate::executor::jsonb::value_to_jb(&args[1]);
                let mut buf = Vec::new();
                crate::executor::jsonb::encode_elem_pub(&mut buf, &key);
                crate::executor::jsonb::encode_elem_pub(&mut buf, &val);
                Value::Blob(buf)
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
            // func.c instrFunc: BLOB×BLOB searches bytes and counts
            // bytes; every other pairing searches the TEXT forms and
            // counts CHARACTERS (`instr('héllo','l')` is 3). An empty
            // needle is found at 1.
            if let (Value::Blob(h), Value::Blob(n)) = (&args[0], &args[1]) {
                if n.is_empty() {
                    return Ok(Value::Integer(1));
                }
                return Ok(Value::Integer(
                    h.windows(n.len())
                        .position(|w| w == n.as_slice())
                        .map_or(0, |p| p as i64 + 1),
                ));
            }
            let hay: Vec<u8> = match &args[0] {
                Value::Blob(b) => b.clone(),
                v => v.as_text().into_bytes(),
            };
            let needle: Vec<u8> = match &args[1] {
                Value::Blob(b) => b.clone(),
                v => v.as_text().into_bytes(),
            };
            if needle.is_empty() {
                return Ok(Value::Integer(1));
            }
            match hay
                .windows(needle.len())
                .position(|w| w == needle.as_slice())
            {
                Some(pos) => Value::Integer(
                    hay[..pos].iter().filter(|&&b| b & 0xc0 != 0x80).count() as i64 + 1,
                ),
                None => Value::Integer(0),
            }
        }
        // PRINTF — minimal SQLite printf implementation. Supports %d, %s,
        // %f, %x, %c, %% substitutions. NULL format returns NULL.
        // printf.c sqlite3_str_vappendf in SQL-function mode — see
        // executor/printf.rs. A NULL (or absent) format yields NULL.
        "printf" | "format" => match args.first() {
            None | Some(Value::Null) => Value::Null,
            Some(f) => {
                let fmt: Vec<u8> = match f {
                    Value::Blob(b) => b.clone(),
                    v => v.as_text().into_bytes(),
                };
                crate::executor::printf::sql_printf(&fmt, &args[1..])
            }
        },
        // MIN(a, b, c, ...) — scalar form (not the aggregate form).
        // Returns the smallest argument. SQLite semantics: if ANY arg is
        // NULL, the result is NULL (the comparison short-circuits).
        "min" if args.len() > 1 => {
            if args.iter().any(|v| v.is_null()) {
                return Ok(Value::Null);
            }
            // func.c minmaxFunc: `(cmp(best, v) ^ mask) >= 0` with mask 0
            // for min — a TIE moves to the LATER argument (`min(1, 1.0)`
            // is 1.0); max keeps the earlier one.
            let mut best: Option<Value> = None;
            for v in args {
                if best.is_none()
                    || crate::executor::value_cmp_conn(v, best.as_ref().unwrap())
                        != std::cmp::Ordering::Greater
                {
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
                if best.is_none()
                    || crate::executor::value_cmp_conn(v, best.as_ref().unwrap())
                        == std::cmp::Ordering::Greater
                {
                    best = Some(v.clone());
                }
            }
            best.unwrap_or(Value::Null)
        }
        // SIGN(x) — returns -1, 0, or +1 depending on the sign of x.
        // NULL input returns NULL.
        // func.c signFunc: only arguments whose sqlite3_value_numeric_type
        // is INTEGER or REAL have a sign — non-numeric TEXT and every
        // BLOB yield NULL (`sign('abc')`, `sign(x'01')`).
        "sign" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => {
                let r = v.as_real();
                Value::Integer(if r < 0.0 {
                    -1
                } else if r > 0.0 {
                    1
                } else {
                    0
                })
            }
        },
        // POWER(x, y) / POW(x, y) — x^y.
        "power" | "pow" => match (
            args.first().and_then(numeric_value),
            args.get(1).and_then(numeric_value),
        ) {
            (Some(a), Some(b)) => real_result(a.as_real().powf(b.as_real())),
            _ => Value::Null,
        },
        // MOD(x, y) — remainder (integer flavor preserved).
        // func.c math2Func(fmod): both operands must be numeric
        // (sqlite3_value_numeric_type), the result is always REAL, and a
        // NaN result (`mod(x, 0)`) is NULL.
        "mod" => match (
            args.first().and_then(numeric_value),
            args.get(1).and_then(numeric_value),
        ) {
            (Some(a), Some(b)) => real_result(a.as_real() % b.as_real()),
            _ => Value::Null,
        },
        // Trigonometry (SQLite's MATH functions extension — always on here).
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh"
        | "acosh" | "atanh" => match args.first().and_then(numeric_value) {
            None => Value::Null,
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
                real_result(r)
            }
        },
        "atan2" => match (
            args.first().and_then(numeric_value),
            args.get(1).and_then(numeric_value),
        ) {
            (Some(a), Some(b)) => real_result(a.as_real().atan2(b.as_real())),
            _ => Value::Null,
        },
        // DEGREES(x) / RADIANS(x).
        "degrees" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => real_result(v.as_real() * (180.0 / std::f64::consts::PI)),
        },
        "radians" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => real_result(v.as_real() * (std::f64::consts::PI / 180.0)),
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
        "log2" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => {
                let x = v.as_real();
                if x <= 0.0 {
                    Value::Null
                } else {
                    real_result(x.log2())
                }
            }
        },
        // SQRT(x) — math1Func: a negative argument's NaN is NULL.
        "sqrt" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => real_result(v.as_real().sqrt()),
        },
        // FLOOR / CEIL / CEILING / TRUNC — func.c ceilingFunc: an INTEGER
        // (after numeric affinity) passes through unchanged, a REAL is
        // rounded as REAL, anything non-numeric is NULL.
        "floor" | "ceil" | "ceiling" | "trunc" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(Value::Integer(i)) => Value::Integer(i),
            Some(v) => {
                let x = v.as_real();
                real_result(match fname.as_str() {
                    "floor" => x.floor(),
                    "trunc" => x.trunc(),
                    _ => x.ceil(),
                })
            }
        },
        // PI() — 3.141592653589793.
        "pi" => Value::Real(std::f64::consts::PI),
        // EXP(x) — e^x.
        "exp" => match args.first().and_then(numeric_value) {
            None => Value::Null,
            Some(v) => real_result(v.as_real().exp()),
        },
        // LN(x) — natural log.
        // func.c logFunc: the (first) argument must be numeric and > 0.
        // LN = natural log; LOG(X)/LOG10(X) = base 10; LOG(B, X) =
        // ln(X)/ln(B), NULL unless ln(B) > 0 (so bases <= 1 are NULL)
        // and X > 0.
        "ln" | "log" | "log10" => {
            let Some(x0) = args.first().and_then(numeric_value) else {
                return Ok(Value::Null);
            };
            let x0 = x0.as_real();
            if x0 <= 0.0 {
                return Ok(Value::Null);
            }
            if args.len() == 2 && fname != "ln" {
                let b = x0.ln();
                if b <= 0.0 {
                    return Ok(Value::Null);
                }
                let x = args[1].as_real();
                if x <= 0.0 {
                    return Ok(Value::Null);
                }
                return Ok(real_result(x.ln() / b));
            }
            real_result(if fname == "ln" { x0.ln() } else { x0.log10() })
        }
        // ABS already defined above.
        // ZEROBLOB(n) — n zero bytes.
        "zeroblob" => {
            let n = args.first().map(|v| v.as_integer()).unwrap_or(0).max(0);
            if n > SQLITE_MAX_LENGTH {
                return Err(Error::runtime("string or blob too big"));
            }
            Value::Blob(vec![0u8; n as usize])
        }
        // CHAR(c1, c2, ...) — construct a string from code points.
        // func.c charFunc: each argument's int64; anything outside
        // [0, 0x10FFFF] becomes U+FFFD (as do lone surrogates, which a
        // Rust string cannot carry).
        "char" => {
            let mut s = String::new();
            for v in args {
                let x = v.as_integer();
                let ch = if (0..=0x10ffff).contains(&x) {
                    char::from_u32(x as u32).unwrap_or('\u{fffd}')
                } else {
                    '\u{fffd}'
                };
                s.push(ch);
            }
            Value::Text(s.into())
        }
        // UNICODE(s) — code point of the first character of s.
        // func.c unicodeFunc: the C string's first character — an empty
        // string, or one that STARTS with NUL, has none (NULL).
        "unicode" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                match s.chars().next() {
                    Some(c) if c != '\0' => Value::Integer(c as i64),
                    _ => Value::Null,
                }
            }
        },
        // UNISTR(X) — decode \uXXXX escapes (with surrogate pairs) in
        // the text; a malformed \u sequence is an error. Other
        // backslashes pass through verbatim (SQLite: `unistr('a\\b')`
        // keeps both characters).
        "unistr" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                match unistr_decode(&s) {
                    Ok(out) => Value::Text(out.into()),
                    Err(()) => return Err(Error::Runtime("invalid Unicode escape".to_string())),
                }
            }
        },
        // UNISTR_QUOTE(X) — render as a SQL literal that evaluates back to
        // X: a plain 'literal' when no escaping is needed, otherwise
        // `unistr('...')` with \uXXXX escapes for control characters
        // (backslashes and non-ASCII stay raw — SQLite:
        // unistr_quote('a\tb') → unistr('a\u0009b')).
        "unistr_quote" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                let mut body = String::with_capacity(s.len() + 2);
                let mut escaped = false;
                for c in s.chars() {
                    match c {
                        '\'' => body.push_str("''"),
                        c if (c as u32) < 0x20 => {
                            escaped = true;
                            body.push_str(&format!("\\u{:04x}", c as u32));
                        }
                        c => body.push(c),
                    }
                }
                let quoted = format!("'{}'", body);
                Value::Text(if escaped {
                    format!("unistr({})", quoted).into()
                } else {
                    quoted.into()
                })
            }
        },
        // SOUNDEX(X) — the classic Soundex code (letter + 3 digits);
        // input with no letters gives "?000".
        "soundex" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Text(soundex(&v.as_text()).into()),
        },
        "sqlite_compileoption_get" => {
            // 1-BASED index (SQLite: N=1 returns the first option), so
            // PRAGMA compile_options' row order and get() agree.
            let n = args.first().map(|v| v.as_integer()).unwrap_or(0);
            Value::Text(
                compile_options()
                    .get((n - 1).max(0) as usize)
                    .map(|s| (*s).into())
                    .unwrap_or_default(),
            )
        }
        "sqlite_compileoption_used" => {
            let name = args.first().map(|v| v.as_text());
            let name =
                match name {
                    Some(n) if !n.is_empty() => n,
                    _ => return Err(Error::Runtime(
                        "argument to sqlite_compileoption_used() must be a positive length string"
                            .to_string(),
                    )),
                };
            let hit = compile_options().iter().any(|o| {
                o.split_once('=')
                    .map(|(k, _)| k)
                    .unwrap_or(o)
                    .eq_ignore_ascii_case(&name)
            });
            Value::Integer(if hit { 1 } else { 0 })
        }
        // TRUE() / FALSE() — SQLite 3.23+ boolean literals.
        "true" => Value::Integer(1),
        "false" => Value::Integer(0),
        // like(PATTERN, X [, ESCAPE]) — the LIKE operator as a function
        // (argument order matches SQLite: pattern first, then the string).
        // NULL in any operand yields NULL (three-valued logic, like the
        // operator form above).
        "like" => match (args.first(), args.get(1), args.get(2)) {
            (Some(p), Some(v), esc) if !p.is_null() && !v.is_null() => {
                let esc_val = match esc {
                    Some(e) if !e.is_null() => {
                        if like_escape_char(e).is_none() {
                            return Err(Error::runtime(
                                "ESCAPE expression must be a single character",
                            ));
                        }
                        Some(e.clone())
                    }
                    Some(_) => return Ok(Value::Null),
                    None => None,
                };
                Value::Integer(i64::from(like_match(v, p, esc_val.as_ref(), false)))
            }
            _ => Value::Null,
        },
        // likelihood(X, P) — a query-planner hint that evaluates to X.
        // P must be a constant in [0.0, 1.0]; out-of-range P is an error
        // (SQLite: "second argument to likelihood() must be a constant
        // between 0.0 and 1.0"). Eval-time validation catches literal
        // out-of-range values.
        "likelihood" => {
            if let Some(p) = args.get(1) {
                let ok = match p {
                    Value::Integer(i) => *i == 0 || *i == 1,
                    Value::Real(f) => (0.0..=1.0).contains(f),
                    _ => false,
                };
                if !ok {
                    return Err(Error::semantic(
                        "second argument to likelihood() must be a constant between 0.0 and 1.0"
                            .to_string(),
                    ));
                }
            }
            match args.first() {
                Some(v) => v.clone(),
                None => Value::Null,
            }
        }
        // pg_typeof(X) — PostgreSQL's type-name introspection, mapped to
        // the closest PG type of our storage classes: NULL → 'unknown',
        // INTEGER → 'integer', REAL → 'double precision' (SQLite REAL is
        // an IEEE float8; PG 'real' is float4), TEXT → 'text', BLOB →
        // 'bytea'. (The SQLite-native typeof() keeps its storage-class
        // names.)
        "pg_typeof" => Value::Text(
            match args.first() {
                Some(Value::Null) | None => "unknown",
                Some(Value::Integer(_)) => "integer",
                Some(Value::Real(_)) => "double precision",
                Some(Value::Text(_)) => "text",
                Some(Value::Blob(_)) => "bytea",
            }
            .into(),
        ),
        // JSON1 — see json.rs. USER FUNCTIONS take priority over JSON1
        // so extensions can shadow built-in JSON names (SQLite: user
        // functions override core ones registered in the same "override"
        // slot). Unknown names error like SQLite ("no such function").
        // Full-text search (to_tsvector family) and geospatial (ST_*
        // family) dispatch the same way — after user functions (they can
        // be shadowed by plugins), before the final error.
        _ => {
            if let Some(r) = crate::plugin::call_user_scalar(&fname, args) {
                return r;
            }
            match crate::executor::json::call_json_function(&fname, args)? {
                Some(v) => v,
                None => match crate::executor::fts::call_fts_function(&fname, args)? {
                    Some(v) => v,
                    None => match crate::executor::geo::call_geo_function(&fname, args)? {
                        Some(v) => v,
                        None => match crate::executor::regex::call_regex_function(&fname, args)? {
                            Some(v) => v,
                            None => {
                                match crate::executor::trgm::call_trgm_function(&fname, args)? {
                                    Some(v) => v,
                                    None => {
                                        match crate::plugin::geopoly::call_geopoly_function(
                                            &fname, args,
                                        )? {
                                            Some(v) => v,
                                            None => {
                                                return Err(Error::NotFound(format!(
                                                    "no such function: {}",
                                                    name
                                                )));
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    },
                },
            }
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
        "concat",
        "concat_ws",
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
        "geopoly_area",
        "geopoly_bbox",
        "geopoly_blob",
        "geopoly_ccw",
        "geopoly_contains_point",
        "geopoly_debug",
        "geopoly_group_bbox",
        "geopoly_json",
        "geopoly_overlap",
        "geopoly_regular",
        "geopoly_svg",
        "geopoly_within",
        "geopoly_xform",
        "glob",
        "group_concat",
        "hex",
        "if",
        "ifnull",
        "iif",
        "instr",
        "json",
        "json_array",
        "json_array_length",
        "json_error_position",
        "json_extract",
        "json_pretty",
        "json_insert",
        "json_object",
        "json_patch",
        "json_quote",
        "json_remove",
        "json_replace",
        "json_set",
        "json_valid",
        "jsonb",
        "jsonb_array",
        "jsonb_extract",
        "jsonb_group_array",
        "jsonb_group_object",
        "jsonb_insert",
        "jsonb_object",
        "jsonb_patch",
        "jsonb_remove",
        "jsonb_replace",
        "jsonb_set",
        "show_trgm",
        "similarity",
        "soundex",
        "sqlite_compileoption_get",
        "sqlite_compileoption_used",
        "unistr",
        "unistr_quote",
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
        "numnode",
        "octet_length",
        "pg_typeof",
        "phraseto_tsquery",
        "pi",
        "plainto_tsquery",
        "power",
        "printf",
        "quote",
        "random",
        "randomblob",
        "rank",
        "regexp",
        "regexp_count",
        "regexp_instr",
        "regexp_like",
        "regexp_replace",
        "regexp_substr",
        "replace",
        "round",
        "row_number",
        "rtrim",
        "sign",
        "sqlite_version",
        "sqlite_source_id",
        "sqrt",
        "st_area",
        "st_asgeojson",
        "st_astext",
        "st_centroid",
        "st_convexhull",
        "st_contains",
        "st_crosses",
        "st_disjoint",
        "st_distancesphere",
        "st_distancespheroid",
        "st_dwithin",
        "st_endpoint",
        "st_envelope",
        "st_equals",
        "st_expand",
        "st_exteriorring",
        "st_geometrytype",
        "st_geomfromtext",
        "st_interiorringn",
        "st_intersects",
        "st_isclosed",
        "st_isring",
        "st_isvalid",
        "st_length",
        "st_makeline",
        "st_makeenvelope",
        "st_makepoint",
        "st_npoints",
        "st_numinteriorrings",
        "st_numpoints",
        "st_overlaps",
        "st_perimeter",
        "st_point",
        "st_pointn",
        "st_reverse",
        "st_rotate",
        "st_scale",
        "st_setsrid",
        "st_srid",
        "st_startpoint",
        "st_touches",
        "st_translate",
        "st_within",
        "st_x",
        "st_y",
        "strftime",
        "strip",
        "substr",
        "substring",
        "sum",
        "time",
        "timediff",
        "to_tsquery",
        "to_tsvector",
        "total",
        "total_changes",
        "trim",
        "true",
        "trunc",
        "ts_headline",
        "ts_match",
        "ts_rank",
        "ts_rank_cd",
        "tsvector_concat",
        "typeof",
        "unicode",
        "unixepoch",
        "upper",
        "websearch_to_tsquery",
        "word_similarity",
        "zeroblob",
    ];
    let lowered = name.to_ascii_lowercase();
    BUILTIN.binary_search(&lowered.as_str()).is_ok()
}

fn quote_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Integer(i) => i.to_string(),
        // quoteFunc renders REALs with the zero-pad flag, whose infinity
        // spelling is the re-parseable 9.0e+999 (not "Inf").
        Value::Real(f) if f.is_infinite() => {
            if *f < 0.0 {
                "-9.0e+999".to_string()
            } else {
                "9.0e+999".to_string()
            }
        }
        Value::Real(f) => crate::types::format_real(*f),
        // quoteFunc renders TEXT through `%Q`, which stops at the first
        // NUL (C string): quote('a' || char(0) || 'b') is 'a'.
        Value::Text(s) => {
            let s = s.split('\0').next().unwrap_or("");
            format!("'{}'", s.replace('\'', "''"))
        }
        Value::Blob(b) => format!(
            "X'{}'",
            b.iter().map(|x| format!("{:02X}", x)).collect::<String>()
        ),
    }
}

/// SQLITE_MAX_LENGTH: the default cap on any string or BLOB a function
/// may materialize ("string or blob too big") — without it
/// `zeroblob(1e18)` would abort the process on allocation.
pub(crate) const SQLITE_MAX_LENGTH: i64 = 1_000_000_000;

/// `sqlite3_value_numeric_type` as a value: INTEGER and REAL pass, TEXT
/// takes numeric affinity WITHOUT the integer squeeze (`'5'` → 5, `'1.0'`
/// → 1.0, `'abc'` / `'5x'` → not numeric), NULL and BLOB are not numeric.
pub(crate) fn numeric_value(v: &Value) -> Option<Value> {
    match v {
        Value::Integer(_) | Value::Real(_) => Some(v.clone()),
        Value::Text(t) => {
            let z = t.as_bytes();
            let (r, rc) = crate::types::numeric::atof(z);
            if rc <= 0 {
                return None;
            }
            if rc == 1 {
                let ix = crate::types::numeric::real_to_i64(r);
                if crate::types::numeric::real_same_as_int(r, ix) {
                    return Some(Value::Integer(ix));
                }
                let (i, irc) = crate::types::numeric::atoi64(z);
                if irc == 0 {
                    return Some(Value::Integer(i));
                }
            }
            Some(Value::Real(r))
        }
        _ => None,
    }
}

/// `sqlite3_result_double`: a NaN result is stored as NULL.
#[inline]
pub(crate) fn real_result(r: f64) -> Value {
    if r.is_nan() {
        Value::Null
    } else {
        Value::Real(r)
    }
}

/// Process-wide pseudo-random generator for random() / randomblob():
/// xoshiro256** per thread, seeded from the OS-keyed SipHash state
/// (`RandomState` draws its keys from the OS RNG), the clock and the
/// thread identity. The previous generator was a fixed LCG over the
/// wall clock (two calls in one nanosecond collided) and randomblob()
/// emitted a CONSTANT byte pattern — `hex(randomblob(16))` ids collided
/// on every call.
fn with_rng<R>(f: impl FnOnce(&mut [u64; 4]) -> R) -> R {
    use std::cell::RefCell;
    use std::hash::{BuildHasher, Hasher};
    thread_local! {
        static STATE: RefCell<Option<[u64; 4]>> = const { RefCell::new(None) };
    }
    STATE.with(|cell| {
        let mut st = cell.borrow_mut();
        let s = st.get_or_insert_with(|| {
            let mut seed = [0u64; 4];
            for (k, slot) in seed.iter_mut().enumerate() {
                let mut h = std::collections::hash_map::RandomState::new().build_hasher();
                h.write_usize(k);
                h.write_u128(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                );
                h.write(format!("{:?}", std::thread::current().id()).as_bytes());
                *slot = h.finish() | 1;
            }
            seed
        });
        f(s)
    })
}

fn next_random(s: &mut [u64; 4]) -> u64 {
    let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
    let t = s[1] << 17;
    s[2] ^= s[0];
    s[3] ^= s[1];
    s[1] ^= s[2];
    s[0] ^= s[3];
    s[2] ^= t;
    s[3] = s[3].rotate_left(45);
    result
}

fn rand_i64() -> i64 {
    with_rng(|s| next_random(s) as i64)
}

fn fill_random(out: &mut [u8]) {
    with_rng(|s| {
        for chunk in out.chunks_mut(8) {
            let v = next_random(s).to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    })
}

/// Extract a COLLATE name from one side of a comparison (SQLite allows
/// `a COLLATE X < b` and `a < b COLLATE X`; the RHS wins when both sides
/// specify one).
pub(crate) fn comparison_collation(e: &Expr) -> Option<String> {
    match e {
        Expr::Collate { collation, .. } => Some(collation.clone()),
        // The IN-subquery affinity carrier wraps the operand (and any
        // COLLATE on it) — see IN_RHS_MARKER.
        Expr::Cast { expr, type_name } if type_name.starts_with(IN_RHS_MARKER) => {
            comparison_collation(expr)
        }
        _ => None,
    }
}

/// Comparison through a collation (text-text pairs only; other types keep
/// the engine's total order). Result mirrors `apply_binary` for the six
/// comparison operators.
pub(crate) fn apply_binary_collated(
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
        // JSON path operators are intercepted in `evaluate` (they can
        // raise); reaching here means a fallback context — treat as NULL.
        // Same for the FTS `@@` and KNN `<->` operators (their RAISE-free
        // compiled fallbacks simply never run — the join compiler declines
        // them, see executor/mod.rs compile_join_jexpr).
        Arrow | ArrowText | FtsMatch | Distance => Value::Null,
        // Integer overflow PROMOTES TO REAL (SQLite: 9223372036854775807 + 1
        // is 9.223372036854776e18, never a wrapped i64).
        Add | Sub | Mul | Div | Mod => sqlite_arith(op, l, r),
        Concat => l.concat(r),
        BitAnd | BitOr | BitXor | ShiftLeft | ShiftRight => sqlite_bitop(op, l, r),
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
        // Range comparisons order TEXT×TEXT pairs under the connection's
        // file encoding (SQLite's BINARY collation: memcmp of the encoded
        // bytes — on UTF-16 files that is neither code-unit nor code-point
        // order). UTF-8 connections take `Value::cmp` untouched; other
        // type classes order identically under both (class-first).
        // Equality is NOT re-routed: both orders induce the same equality
        // relation, and `PartialEq` is the cheaper test.
        Lt => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(
                    if crate::executor::value_cmp_conn(l, r) == std::cmp::Ordering::Less {
                        1
                    } else {
                        0
                    },
                )
            }
        }
        LtEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(
                    if crate::executor::value_cmp_conn(l, r) != std::cmp::Ordering::Greater {
                        1
                    } else {
                        0
                    },
                )
            }
        }
        Gt => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(
                    if crate::executor::value_cmp_conn(l, r) == std::cmp::Ordering::Greater {
                        1
                    } else {
                        0
                    },
                )
            }
        }
        GtEq => {
            if cmp_operand_missing(l) || cmp_operand_missing(r) {
                Value::Null
            } else {
                Value::Integer(
                    if crate::executor::value_cmp_conn(l, r) != std::cmp::Ordering::Less {
                        1
                    } else {
                        0
                    },
                )
            }
        }
        // SQL three-valued logic for AND / OR (SQLite AND PostgreSQL):
        // NULL is UNKNOWN — a decisive FALSE short-circuits AND to 0, a
        // decisive TRUE short-circuits OR to 1, and every remaining
        // UNKNOWN combination yields NULL. Previously both operators
        // collapsed NULL operands to 0 (`SELECT NULL AND 1` answered 0
        // instead of NULL), diverging from SQLite's documented truth
        // tables.
        And => {
            let l_false = !l.is_null() && !l.is_truthy();
            let r_false = !r.is_null() && !r.is_truthy();
            if l_false || r_false {
                Value::Integer(0)
            } else if l.is_truthy() && r.is_truthy() {
                Value::Integer(1)
            } else {
                Value::Null
            }
        }
        Or => {
            if l.is_truthy() || r.is_truthy() {
                Value::Integer(1)
            } else if (!l.is_null() && !l.is_truthy()) && (!r.is_null() && !r.is_truthy()) {
                Value::Integer(0)
            } else {
                Value::Null
            }
        }
    }
}

/// vdbe.c OP_Add / OP_Subtract / OP_Multiply / OP_Divide / OP_Remainder,
/// verbatim semantics:
///
/// * two INTEGER operands use checked i64 math; overflow (and
///   `i64::MIN / -1`) falls over to REAL math;
/// * NULL anywhere → NULL;
/// * TEXT / BLOB operands read through `numericType` (`'12abc'` → 12,
///   `'1.5'` → 1.5, `x'3132'` → 12); if both then read as INTEGER the
///   integer path runs, otherwise REAL math;
/// * division / remainder by zero → NULL (REAL division checks the REAL
///   divisor: `1 / 0.5` is 2.0, not a "zero" integer divisor);
/// * REAL remainder truncates both operands to INTEGER and yields a REAL
///   (`5.5 % 2` → 1.0);
/// * a NaN result (`inf - inf`) → NULL.
pub(crate) fn sqlite_arith(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    fn int_math(op: BinaryOp, a: i64, b: i64) -> Option<Value> {
        // a = left, b = right. `None` = overflow → REAL math.
        Some(match op {
            Add => Value::Integer(a.checked_add(b)?),
            Sub => Value::Integer(a.checked_sub(b)?),
            Mul => Value::Integer(a.checked_mul(b)?),
            Div => {
                if b == 0 {
                    return Some(Value::Null);
                }
                if b == -1 && a == i64::MIN {
                    return None;
                }
                Value::Integer(a / b)
            }
            _ => {
                if b == 0 {
                    return Some(Value::Null);
                }
                let b = if b == -1 { 1 } else { b };
                Value::Integer(a % b)
            }
        })
    }
    // `l`/`r` are the ORIGINAL operands: REAL math reads them through
    // sqlite3VdbeRealValue / sqlite3VdbeIntValue on the un-converted
    // memory cells (OP_Remainder's `iA = sqlite3VdbeIntValue(pIn1)` takes
    // the TEXT's integer PREFIX: `-21 % '9.2e18'` is -21 % 9 = -3.0).
    fn fp_math(op: BinaryOp, l: &Value, r: &Value) -> Value {
        let (a, b) = (l.as_real(), r.as_real());
        let out = match op {
            Add => a + b,
            Sub => a - b,
            Mul => a * b,
            Div => {
                if b == 0.0 {
                    return Value::Null;
                }
                a / b
            }
            _ => {
                let ia = l.as_integer();
                let ib = r.as_integer();
                if ib == 0 {
                    return Value::Null;
                }
                let ib = if ib == -1 { 1 } else { ib };
                (ia % ib) as f64
            }
        };
        if out.is_nan() {
            Value::Null
        } else {
            Value::Real(out)
        }
    }
    if let (Value::Integer(a), Value::Integer(b)) = (l, r) {
        return int_math(op, *a, *b).unwrap_or_else(|| fp_math(op, l, r));
    }
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let ln = l.to_numeric();
    let rn = r.to_numeric();
    if let (Value::Integer(a), Value::Integer(b)) = (&ln, &rn) {
        return int_math(op, *a, *b).unwrap_or_else(|| fp_math(op, l, r));
    }
    fp_math(op, l, r)
}

/// vdbe.c OP_BitAnd / OP_BitOr / OP_ShiftLeft / OP_ShiftRight: operands
/// read through `sqlite3VdbeIntValue` (prefix integer parse for TEXT and
/// BLOB, clamping truncation for REAL); a negative shift count shifts
/// the other way; a count >= 64 yields 0 (or -1 for a right shift of a
/// negative value); right shifts sign-extend.
pub(crate) fn sqlite_bitop(op: BinaryOp, l: &Value, r: &Value) -> Value {
    use BinaryOp::*;
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let a = l.as_integer();
    let b = r.as_integer();
    Value::Integer(match op {
        BitAnd => a & b,
        BitOr => a | b,
        BitXor => a ^ b,
        _ => {
            if b == 0 {
                a
            } else {
                let mut left = matches!(op, ShiftLeft);
                let mut n = b;
                if n < 0 {
                    left = !left;
                    n = if n > -64 { -n } else { 64 };
                }
                if n >= 64 {
                    if a >= 0 || left {
                        0
                    } else {
                        -1
                    }
                } else if left {
                    ((a as u64) << n) as i64
                } else {
                    a >> n
                }
            }
        }
    })
}

pub fn apply_unary(op: UnaryOp, v: &Value) -> Value {
    match op {
        // SQLite codes a non-literal unary minus as `0 - x` (OP_Subtract),
        // so TEXT/BLOB operands go through numericType: -'5' is -5,
        // -'1.5' is -1.5, -'abc' is 0, -x'35' is -5, and -i64::MIN
        // promotes to REAL. (Negative literals never reach here as text.)
        UnaryOp::Neg => match v {
            Value::Null => Value::Null,
            _ => sqlite_arith(BinaryOp::Sub, &Value::Integer(0), v),
        },
        UnaryOp::Pos => v.clone(),
        // SQLite three-valued logic: NOT NULL is NULL; every other value
        // flips under the SAME truthiness rule WHERE uses
        // (`sqlite3VdbeBooleanValue`: NOT x'31' is 0, NOT x'41' is 1).
        UnaryOp::Not => match v {
            Value::Null => Value::Null,
            v => Value::Integer(if v.is_truthy() { 0 } else { 1 }),
        },
        // OP_BitNot: `~sqlite3VdbeIntValue(x)` (~x'31' is -2).
        UnaryOp::BitNot => {
            if v.is_null() {
                Value::Null
            } else {
                Value::Integer(!v.as_integer())
            }
        }
        // OP_IsTrue: sqlite3VdbeBooleanValue(x, NULL-as) ^ negated, where a
        // NULL counts as the OPPOSITE of the keyword tested (`NULL IS
        // FALSE` is 0, `NULL IS NOT TRUE` is 1).
        UnaryOp::Truth { truth, negated } => {
            let b = if v.is_null() { !truth } else { v.is_truthy() };
            Value::Integer(i64::from((b == truth) ^ negated))
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
                // C-string operands: both end at their first NUL byte.
                let cstr =
                    |b: &'_ [u8]| -> usize { b.iter().position(|&c| c == 0).unwrap_or(b.len()) };
                let (sb, pb) = (sv.as_str().as_bytes(), pv.as_str().as_bytes());
                if let Some(hit) =
                    like_match_bytes(&sb[..cstr(sb)], &pb[..cstr(pb)], case_sensitive)
                {
                    return hit;
                }
                // None = general wildcard shape on a NON-ASCII subject
                // (`_` must match one CHARACTER, not one byte): the
                // char-based path below handles it.
            }
        }
    }
    // General path: ESCAPE support, non-TEXT operands (numeric LIKE casts
    // to text), non-ASCII patterns — func.c likeFunc + patternCompare.
    let s = like_operand_bytes(value);
    let p = like_operand_bytes(pattern);
    let mut info = PatternInfo {
        match_all: u32::from(b'%'),
        match_one: u32::from(b'_'),
        match_set: 0,
        no_case: !case_sensitive,
    };
    let mut esc = 0u32;
    if let Some(e) = escape {
        // Callers validate the single-character rule (see
        // `like_escape_char`); a bad value here degrades to "no escape".
        if let Some(c) = like_escape_char(e) {
            esc = c;
            if esc == info.match_all {
                info.match_all = 0;
            }
            if esc == info.match_one {
                info.match_one = 0;
            }
        }
    }
    pattern_compare(&p, &s, &info, esc) == PATTERN_MATCH
}

/// The bytes LIKE/GLOB see for an operand (`sqlite3_value_text`): TEXT
/// as-is, BLOB bytes raw, numbers rendered; truncated at the first NUL
/// (the C string walk).
fn like_operand_bytes(v: &Value) -> Vec<u8> {
    let mut b = match v {
        Value::Text(t) => t.as_bytes().to_vec(),
        Value::Blob(b) => b.clone(),
        other => other.as_text().into_bytes(),
    };
    if let Some(z) = b.iter().position(|&c| c == 0) {
        b.truncate(z);
    }
    b
}

/// The LIKE ESCAPE character: `Some(cp)` iff the value's text is exactly
/// one UTF-8 character (SQLite: "ESCAPE expression must be a single
/// character" otherwise).
pub(crate) fn like_escape_char(v: &Value) -> Option<u32> {
    let t = v.as_text();
    let mut it = t.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c as u32),
        _ => None,
    }
}

struct PatternInfo {
    match_all: u32,
    match_one: u32,
    match_set: u32,
    no_case: bool,
}

const PATTERN_MATCH: i32 = 0;
const PATTERN_NOMATCH: i32 = 1;
const PATTERN_NOWILDCARDMATCH: i32 = 2;

/// sqlite3Utf8Read: decode one (leniently validated) UTF-8 character
/// starting at `*i`; 0 at the end of the input.
fn utf8_read(z: &[u8], i: &mut usize) -> u32 {
    if *i >= z.len() {
        return 0;
    }
    let mut c = u32::from(z[*i]);
    *i += 1;
    if c >= 0xc0 {
        const TRANS: [u8; 64] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x00, 0x01, 0x02, 0x03, 0x00, 0x01, 0x00, 0x00,
        ];
        c = u32::from(TRANS[(c - 0xc0) as usize]);
        while *i < z.len() && (z[*i] & 0xc0) == 0x80 {
            c = (c << 6) + u32::from(0x3f & z[*i]);
            *i += 1;
        }
        if c < 0x80 || (c & 0xFFFF_F800) == 0xD800 || (c & 0xFFFF_FFFE) == 0xFFFE {
            c = 0xFFFD;
        }
    }
    c
}

#[inline]
fn ascii_lower(c: u32) -> u32 {
    if (u32::from(b'A')..=u32::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

/// func.c `patternCompare`, verbatim semantics (LIKE and GLOB): `%`/`*`
/// runs collapse, the NOWILDCARDMATCH return prunes the search to
/// polynomial time (the old recursive matcher was exponential on
/// `'%a%a%a%a%b'` shapes), case folding is ASCII-only, `[...]` sets for
/// GLOB, and an escaped character is always literal.
fn pattern_compare(pat: &[u8], s: &[u8], info: &PatternInfo, match_other: u32) -> i32 {
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut escaped_at: Option<usize> = None;
    loop {
        let mut c = utf8_read(pat, &mut pi);
        if c == 0 {
            break;
        }
        if c == info.match_all && info.match_all != 0 {
            loop {
                c = utf8_read(pat, &mut pi);
                if c == info.match_all && info.match_all != 0 {
                    continue;
                }
                if c == info.match_one && info.match_one != 0 {
                    if utf8_read(s, &mut si) == 0 {
                        return PATTERN_NOWILDCARDMATCH;
                    }
                    continue;
                }
                break;
            }
            if c == 0 {
                return PATTERN_MATCH;
            } else if c == match_other && match_other != 0 {
                if info.match_set == 0 {
                    c = utf8_read(pat, &mut pi);
                    if c == 0 {
                        return PATTERN_NOWILDCARDMATCH;
                    }
                } else {
                    // "[...]" right after "*": slow recursive search.
                    let rest = &pat[pi - 1..];
                    let mut k = si;
                    while k < s.len() {
                        let b = pattern_compare(rest, &s[k..], info, match_other);
                        if b != PATTERN_NOMATCH {
                            return b;
                        }
                        let mut kk = k;
                        utf8_read(s, &mut kk);
                        k = kk;
                    }
                    return PATTERN_NOWILDCARDMATCH;
                }
            }
            // Search the subject for the first char after the wildcard and
            // recursively continue from each candidate.
            loop {
                let c2 = utf8_read(s, &mut si);
                if c2 == 0 {
                    break;
                }
                let hit = c2 == c
                    || (info.no_case && c < 0x80 && c2 < 0x80 && ascii_lower(c) == ascii_lower(c2));
                if !hit {
                    continue;
                }
                let b = pattern_compare(&pat[pi..], &s[si..], info, match_other);
                if b != PATTERN_NOMATCH {
                    return b;
                }
            }
            return PATTERN_NOWILDCARDMATCH;
        }
        if c == match_other && match_other != 0 {
            if info.match_set == 0 {
                c = utf8_read(pat, &mut pi);
                if c == 0 {
                    return PATTERN_NOMATCH;
                }
                escaped_at = Some(pi);
            } else {
                // GLOB character set.
                let mut prior_c: u32 = 0;
                let mut seen = false;
                let mut invert = false;
                let cs = utf8_read(s, &mut si);
                if cs == 0 {
                    return PATTERN_NOMATCH;
                }
                let mut c2 = utf8_read(pat, &mut pi);
                if c2 == u32::from(b'^') {
                    invert = true;
                    c2 = utf8_read(pat, &mut pi);
                }
                if c2 == u32::from(b']') {
                    if cs == u32::from(b']') {
                        seen = true;
                    }
                    c2 = utf8_read(pat, &mut pi);
                }
                while c2 != 0 && c2 != u32::from(b']') {
                    let next = pat.get(pi).copied().unwrap_or(0);
                    if c2 == u32::from(b'-') && next != b']' && next != 0 && prior_c > 0 {
                        c2 = utf8_read(pat, &mut pi);
                        if cs >= prior_c && cs <= c2 {
                            seen = true;
                        }
                        prior_c = 0;
                    } else {
                        if cs == c2 {
                            seen = true;
                        }
                        prior_c = c2;
                    }
                    c2 = utf8_read(pat, &mut pi);
                }
                if c2 == 0 || !(seen ^ invert) {
                    return PATTERN_NOMATCH;
                }
                continue;
            }
        }
        let c2 = utf8_read(s, &mut si);
        if c == c2 {
            continue;
        }
        if info.no_case && c < 0x80 && c2 < 0x80 && ascii_lower(c) == ascii_lower(c2) {
            continue;
        }
        if c == info.match_one && info.match_one != 0 && escaped_at != Some(pi) && c2 != 0 {
            continue;
        }
        return PATTERN_NOMATCH;
    }
    if si >= s.len() {
        PATTERN_MATCH
    } else {
        PATTERN_NOMATCH
    }
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

/// GLOB matching (func.c globInfo + patternCompare): case-sensitive,
/// `*` / `?` wildcards, `[...]` / `[^...]` sets with ranges.
pub fn glob_match(value: &Value, pattern: &Value) -> bool {
    let s = like_operand_bytes(value);
    let p = like_operand_bytes(pattern);
    let info = PatternInfo {
        match_all: u32::from(b'*'),
        match_one: u32::from(b'?'),
        match_set: u32::from(b'['),
        no_case: false,
    };
    pattern_compare(&p, &s, &info, u32::from(b'[')) == PATTERN_MATCH
}

// ---------------------------------------------------------------------------
// unistr / soundex / compile options
// ---------------------------------------------------------------------------

/// Decode `\uXXXX` escapes (surrogate pairs included). A malformed
/// `\u` sequence is an error; other backslashes copy through verbatim.
fn unistr_decode(s: &str) -> std::result::Result<String, ()> {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            match b.get(i + 1) {
                // \uXXXX — code point escape.
                Some(b'u') => {
                    let hex = s.get(i + 2..i + 6).ok_or(())?;
                    let cp = u32::from_str_radix(hex, 16).map_err(|_| ())?;
                    i += 6;
                    // Lone surrogates decode to U+FFFD (SQLite emits raw
                    // WTF-8 there, which is not valid UTF-8).
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
                // \\ — an escaped backslash.
                Some(b'\\') => {
                    out.push('\\');
                    i += 2;
                }
                // Any other backslash sequence is invalid.
                _ => return Err(()),
            }
        } else {
            // Copy one UTF-8 char.
            let ch = s[i..].chars().next().ok_or(())?;
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// The Soundex letter→digit table (A..Z): 0 = not coded (vowels, H, W, Y).
const SOUNDEX_TABLE: &[u8; 26] = b"01230120022455012623010202";

fn soundex(s: &str) -> String {
    let mut it = s.chars().skip_while(|c| !c.is_ascii_alphabetic());
    let Some(first) = it.next() else {
        return "?000".to_string();
    };
    let up = first.to_ascii_uppercase();
    let mut out = [b'0'; 4];
    out[0] = up as u8;
    let mut k = 1usize;
    let mut last = SOUNDEX_TABLE[(up as u8 - b'A') as usize];
    for c in it {
        if !c.is_ascii_alphabetic() {
            continue;
        }
        let cu = c.to_ascii_uppercase() as u8;
        let code = SOUNDEX_TABLE[(cu - b'A') as usize];
        let is_h_or_w = cu == b'H' || cu == b'W';
        if code == b'0' {
            // Vowels (and Y) reset the collapse; H and W do not.
            if !is_h_or_w {
                last = b'0';
            }
        } else if code != last {
            out[k] = code;
            k += 1;
            if k == 4 {
                break;
            }
            last = code;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Compile options reported by `sqlite_compileoption_get/used`. Mirrors
/// the reference SQLite build the engine is differentially tested
/// against (feature-detection probes like `ENABLE_FTS5` succeed).
pub(crate) const COMPILE_OPTIONS: &[&str] = &[
    "ATOMIC_INTRINSICS=1",
    "COMPILER=rustc",
    "DEFAULT_AUTOVACUUM",
    "DEFAULT_CACHE_SIZE=-2000",
    "DEFAULT_FILE_FORMAT=4",
    "DEFAULT_JOURNAL_SIZE_LIMIT=-1",
    "DEFAULT_MMAP_SIZE=0",
    "DEFAULT_PAGE_SIZE=4096",
    "DEFAULT_PCACHE_INITSZ=20",
    "DEFAULT_RECURSIVE_TRIGGERS",
    "DEFAULT_SECTOR_SIZE=4096",
    "DEFAULT_SYNCHRONOUS=2",
    "DEFAULT_WAL_AUTOCHECKPOINT=1000",
    "DEFAULT_WAL_SYNCHRONOUS=2",
    "DEFAULT_WORKER_THREADS=0",
    "DIRECT_OVERFLOW_READ",
    "ENABLE_DBSTAT_VTAB",
    "ENABLE_FTS3",
    "ENABLE_FTS3_PARENTHESIS",
    "ENABLE_FTS4",
    "ENABLE_FTS5",
    "ENABLE_GEOPOLY",
    "ENABLE_MATH_FUNCTIONS",
    "ENABLE_PERCENTILE",
    "ENABLE_RTREE",
    "MALLOC_SOFT_LIMIT=1024",
    "MAX_ATTACHED=10",
    "MAX_COLUMN=2000",
    "MAX_COMPOUND_SELECT=500",
    "MAX_DEFAULT_PAGE_SIZE=8192",
    "MAX_EXPR_DEPTH=1000",
    "MAX_FUNCTION_ARG=1000",
    "MAX_LENGTH=1000000000",
    "MAX_LIKE_PATTERN_LENGTH=50000",
    "MAX_MMAP_SIZE=0x7fff0000",
    "MAX_PAGE_COUNT=0xfffffffe",
    "MAX_PAGE_SIZE=65536",
    "MAX_SQL_LENGTH=1000000000",
    "MAX_TRIGGER_DEPTH=1000",
    "MAX_VARIABLE_NUMBER=32766",
    "MAX_VDBE_OP=250000000",
    "MAX_WORKER_THREADS=8",
    "MUTEX_PTHREADS",
    "SYSTEM_MALLOC",
    "TEMP_STORE=1",
    "THREADSAFE=1",
];

fn compile_options() -> &'static [&'static str] {
    COMPILE_OPTIONS
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
