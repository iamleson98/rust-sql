//! SQLite JSONB — the raw-preserving JSON pipeline (SQLite 3.45+ / 3.53
//! revised format).
//!
//! SQLite's JSON functions never canonicalize what they don't have to:
//! numbers keep their original text (`1e2` stays `1e2`), strings keep
//! their original escape forms (`"a\u0062"` stays escaped), and the whole
//! document round-trips through the binary JSONB encoding losslessly.
//! This module mirrors that design:
//!
//! * [`Jb`] — a JSON tree that remembers the *raw text* of every number
//!   and the *escape class* of every string (the 13 element subtypes of
//!   the SQLite JSONB spec: INT/INT5/FLOAT/FLOAT5, TEXT/TEXTJ/TEXT5/
//!   TEXTRAW, arrays, objects, null/true/false).
//! * [`parse_lenient`] — SQLite's JSON/JSON5-lenient text parser
//!   (trailing commas, `+1`, `.5`, `1.`, `0x` hex, `Infinity`, `NaN`,
//!   unquoted keys) that preserves raw text and reports 1-based byte
//!   error positions for `json_error_position`.
//! * [`Jb::render`] / [`Jb::render_pretty`] — byte-compatible with
//!   SQLite's `json()` / `json_pretty()` (4-space indent).
//! * [`jb_to_bytes`] / [`jb_from_bytes`] — the JSONB binary codec.
//!   The encoder always emits shortest-form headers (size nibble 0-11,
//!   then 1/2/4/8-byte big-endian extended forms); byte output is
//!   IDENTICAL to SQLite's `jsonb()` for equivalent input, which is what
//!   makes `json_each`'s `id` column (JSONB byte offsets) line up.
//! * [`Sp`] — the spanned decode tree: element byte offsets, used by
//!   `json_each`/`json_tree` to reproduce SQLite's `id`/`parent` ids.
//! * [`value_to_jb`] / [`Jb::to_value`] — SQL-value bridges using
//!   SQLite's `%!.15g`-style REAL rendering (`1.0`, `1.0e+300`).
//!
//! Reference: <https://sqlite.org/jsonb.html> (element types 0-12;
//! sizes 12-15 in the high nibble = 1/2/4/8-byte big-endian size fields).

use crate::types::Value;

// ---------------------------------------------------------------------------
// Element subtypes (SQLite JSONB spec, low nibble of the header byte)
// ---------------------------------------------------------------------------

pub(crate) const T_NULL: u8 = 0;
pub(crate) const T_TRUE: u8 = 1;
pub(crate) const T_FALSE: u8 = 2;
pub(crate) const T_INT: u8 = 3;
pub(crate) const T_INT5: u8 = 4;
pub(crate) const T_FLOAT: u8 = 5;
pub(crate) const T_FLOAT5: u8 = 6;
pub(crate) const T_TEXT: u8 = 7;
pub(crate) const T_TEXTJ: u8 = 8;
pub(crate) const T_TEXT5: u8 = 9;
pub(crate) const T_TEXTRAW: u8 = 10;
pub(crate) const T_ARRAY: u8 = 11;
pub(crate) const T_OBJECT: u8 = 12;

// ---------------------------------------------------------------------------
// The raw-preserving JSON tree
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Jb {
    Null,
    True,
    False,
    /// Canonical RFC 8259 integer text (`-?(0|[1-9][0-9]*)`), any magnitude.
    Int(String),
    /// Non-canonical integer text (hex `0x..`, signed hex).
    Int5(String),
    /// Canonical RFC 8259 real text (`1.5`, `1e2`, `-0.0`, `1E+2`).
    Float(String),
    /// Non-canonical real text (`.5`, `1.`, `.5e1`).
    Float5(String),
    /// String needing no escapes: payload is the raw value.
    Text(String),
    /// String whose payload contains RFC 8259 escape sequences verbatim.
    TextJ(String),
    /// String whose payload contains JSON5 escape sequences.
    Text5(String),
    /// Raw payload that needs escapes inserted when rendered to JSON text.
    TextRaw(String),
    Array(Vec<Jb>),
    /// (key, value) pairs; keys are string elements (types 7-10).
    Object(Vec<(Jb, Jb)>),
}

impl Jb {
    /// SQLite `json_type()` name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Jb::Null => "null",
            Jb::True | Jb::False => "true",
            Jb::Int(_) | Jb::Int5(_) => "integer",
            Jb::Float(_) | Jb::Float5(_) => "real",
            Jb::Text(_) | Jb::TextJ(_) | Jb::Text5(_) | Jb::TextRaw(_) => "text",
            Jb::Array(_) => "array",
            Jb::Object(_) => "object",
        }
    }

    pub fn is_string(&self) -> bool {
        matches!(
            self,
            Jb::Text(_) | Jb::TextJ(_) | Jb::Text5(_) | Jb::TextRaw(_)
        )
    }

    /// Decode a string element to its actual value (escapes resolved).
    pub fn string_value(&self) -> Option<String> {
        match self {
            Jb::Text(s) | Jb::TextRaw(s) => Some(s.clone()),
            Jb::TextJ(s) | Jb::Text5(s) => Some(decode_escapes(s, matches!(self, Jb::Text5(_)))),
            _ => None,
        }
    }

    /// Minified JSON text — SQLite `json()` output.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// Pretty JSON text — SQLite `json_pretty()` output (4-space indent,
    /// `": "` after keys).
    pub fn render_pretty(&self) -> String {
        let mut out = String::new();
        self.write_pretty(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Jb::Null => out.push_str("null"),
            Jb::True => out.push_str("true"),
            Jb::False => out.push_str("false"),
            Jb::Int(s) => out.push_str(s),
            Jb::Int5(s) => out.push_str(&canonical_int5(s)),
            Jb::Float(s) => out.push_str(s),
            Jb::Float5(s) => out.push_str(&canonical_float5(s)),
            Jb::Text(s) => {
                out.push('"');
                out.push_str(s);
                out.push('"');
            }
            Jb::TextJ(s) => {
                // Payload already contains valid RFC 8259 escapes.
                out.push('"');
                out.push_str(s);
                out.push('"');
            }
            Jb::Text5(s) => {
                out.push('"');
                write_escaped_string(&translate_json5_escapes(s), out);
                out.push('"');
            }
            Jb::TextRaw(s) => {
                out.push('"');
                write_escaped_string(s, out);
                out.push('"');
            }
            Jb::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Jb::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    k.write(out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    fn write_pretty(&self, out: &mut String, indent: usize) {
        match self {
            Jb::Array(items) if !items.is_empty() => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    push_indent(out, indent + 1);
                    item.write_pretty(out, indent + 1);
                }
                out.push('\n');
                push_indent(out, indent);
                out.push(']');
            }
            Jb::Object(pairs) if !pairs.is_empty() => {
                out.push_str("{\n");
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(",\n");
                    }
                    push_indent(out, indent + 1);
                    k.write_pretty(out, indent + 1);
                    out.push_str(": ");
                    v.write_pretty(out, indent + 1);
                }
                out.push('\n');
                push_indent(out, indent);
                out.push('}');
            }
            Jb::Array(_) => out.push_str("[]"),
            Jb::Object(_) => out.push_str("{}"),
            other => other.write(out),
        }
    }

    /// SQL value — SQLite `json_extract`/`->>` semantics.
    pub fn to_value(&self) -> Value {
        match self {
            Jb::Null => Value::Null,
            Jb::True => Value::Integer(1),
            Jb::False => Value::Integer(0),
            Jb::Int(s) => int_text_to_value(s),
            Jb::Int5(s) => int_text_to_value(&canonical_int5(s)),
            Jb::Float(s) => num_text_to_real(s)
                .map(Value::Real)
                .unwrap_or_else(|| Value::Text(s.as_str().into())),
            Jb::Float5(s) => num_text_to_real(&canonical_float5(s))
                .map(Value::Real)
                .unwrap_or_else(|| Value::Text(s.as_str().into())),
            Jb::Text(s) | Jb::TextRaw(s) => Value::Text(s.as_str().into()),
            Jb::TextJ(s) => Value::Text(decode_escapes(s, false).into()),
            Jb::Text5(s) => Value::Text(decode_escapes(s, true).into()),
            Jb::Array(_) | Jb::Object(_) => Value::Text(self.render().into()),
        }
    }
}

fn push_indent(out: &mut String, n: usize) {
    for _ in 0..n {
        out.push_str("    ");
    }
}

/// Integer text (any magnitude) → SQL value: i64 when it fits, else REAL
/// (SQLite: `typeof(json_extract('[9223372036854775808]','$'))` = 'real').
fn int_text_to_value(s: &str) -> Value {
    if let Ok(i) = s.parse::<i64>() {
        Value::Integer(i)
    } else {
        num_text_to_real(s)
            .map(Value::Real)
            .unwrap_or_else(|| Value::Text(s.into()))
    }
}

/// Parse a numeric text to f64 (accepts the JSON5 forms via canonicalization
/// by the caller).
fn num_text_to_real(s: &str) -> Option<f64> {
    s.parse::<f64>()
        .ok()
        .filter(|f| f.is_finite() || s.contains("999"))
}

// ---------------------------------------------------------------------------
// Lenient text parser (SQLite's JSON + JSON5 superset)
// ---------------------------------------------------------------------------

/// Parse with SQLite's default leniency. `Err(pos)` is the 1-based byte
/// position of the syntax error (`json_error_position` semantics).
pub fn parse_lenient(s: &str) -> Result<Jb, usize> {
    let mut p = JParser {
        b: s.as_bytes(),
        pos: 0,
        strict: false,
    };
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.b.len() {
        return Err(p.pos + 1);
    }
    Ok(v)
}

/// Parse under strict RFC 8259 rules (json_valid bit 1): no trailing
/// commas, no `+`/`.5`/`1.`/hex/Infinity/NaN, no unquoted keys.
pub fn parse_strict(s: &str) -> Result<Jb, usize> {
    let mut p = JParser {
        b: s.as_bytes(),
        pos: 0,
        strict: true,
    };
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.b.len() {
        return Err(p.pos + 1);
    }
    Ok(v)
}

/// Parse under JSON5 rules (json_valid bit 2) — same as lenient today
/// (the lenient parser already accepts the JSON5 superset SQLite does).
pub fn parse_json5(s: &str) -> Result<Jb, usize> {
    parse_lenient(s)
}

struct JParser<'a> {
    b: &'a [u8],
    pos: usize,
    strict: bool,
}

impl<'a> JParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn err(&self) -> usize {
        self.pos + 1
    }

    fn parse_value(&mut self) -> Result<Jb, usize> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(|(v, _)| v),
            Some(b't') => self.parse_lit("true", Jb::True),
            Some(b'f') => self.parse_lit("false", Jb::False),
            Some(b'n') => self.parse_lit("null", Jb::Null),
            Some(b'N') if !self.strict => self.parse_lit("NaN", Jb::Null),
            Some(b'I') if !self.strict => self.parse_lit("Infinity", Jb::Float("9e999".into())),
            Some(b'-') | Some(b'+') | Some(b'0'..=b'9') | Some(b'.') => self.parse_number(),
            _ => Err(self.err()),
        }
    }

    fn parse_lit(&mut self, lit: &str, v: Jb) -> Result<Jb, usize> {
        if self.b[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(v)
        } else {
            Err(self.err())
        }
    }

    fn parse_number(&mut self) -> Result<Jb, usize> {
        let start = self.pos;
        // Optional sign. `-Infinity` / `-NaN` are accepted in lenient mode.
        match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                if !self.strict && self.b[self.pos..].starts_with(b"Infinity") {
                    self.pos += 8;
                    return Ok(Jb::Float("-9e999".into()));
                }
                if !self.strict && self.b[self.pos..].starts_with(b"NaN") {
                    self.pos += 3;
                    return Ok(Jb::Null);
                }
            }
            Some(b'+') => {
                if self.strict {
                    return Err(self.err());
                }
                self.pos += 1;
                if self.b[self.pos..].starts_with(b"Infinity") {
                    self.pos += 8;
                    return Ok(Jb::Float("9e999".into()));
                }
            }
            _ => {}
        }
        // Hex? (JSON5, after optional sign)
        if self.pos + 1 < self.b.len()
            && self.b[self.pos] == b'0'
            && (self.b[self.pos + 1] == b'x' || self.b[self.pos + 1] == b'X')
        {
            if self.strict {
                return Err(self.err());
            }
            self.pos += 2;
            let hs = self.pos;
            while matches!(
                self.peek(),
                Some(b'0'..=b'9') | Some(b'a'..=b'f') | Some(b'A'..=b'F')
            ) {
                self.pos += 1;
            }
            if self.pos == hs {
                return Err(self.pos + 1);
            }
            let text = std::str::from_utf8(&self.b[start..self.pos])
                .unwrap()
                .to_string();
            return Ok(Jb::Int5(text));
        }
        // Integer part (may be empty for `.5` in lenient mode).
        let int_start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let int_digits = self.pos - int_start;
        // Fraction
        let mut frac_start = None;
        let mut frac_digits = 0usize;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            frac_start = Some(self.pos);
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            frac_digits = self.pos - frac_start.unwrap();
        }
        // Exponent
        let mut has_exp = false;
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            let es = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == es {
                self.pos = save; // `1e` without digits: rewind and error
                return Err(self.pos + 1);
            }
            has_exp = true;
        }
        if int_digits == 0 && frac_digits == 0 {
            return Err(start + 1);
        }
        let text = std::str::from_utf8(&self.b[start..self.pos]).unwrap();
        // Leading zeros: RFC 8259 forbids `01`, `00` — SQLite rejects even
        // leniently (`json('[00]')` errors).
        if int_digits > 1 && self.b[int_start] == b'0' {
            return Err(int_start + 1);
        }
        if frac_start.is_some() || has_exp {
            // Real number: FLOAT when canonical RFC 8259, else FLOAT5.
            // Canonical: no '+', nonempty int part, frac (when present)
            // has digits, exp (when present) has digits (checked above).
            let canonical = !text.starts_with('+')
                && int_digits > 0
                && (frac_start.is_none() || frac_digits > 0);
            if self.strict && !canonical {
                return Err(start + 1);
            }
            if canonical {
                Ok(Jb::Float(text.to_string()))
            } else {
                // Strip a leading '+' (SQLite canonicalizes "+.5" to ".5").
                let t = text.strip_prefix('+').unwrap_or(text);
                Ok(Jb::Float5(t.to_string()))
            }
        } else {
            // Pure integer: canonical INT (a leading '+' is stripped;
            // magnitude does not matter — JSONB INT holds arbitrary
            // precision text).
            let t = text.strip_prefix('+').unwrap_or(text);
            Ok(Jb::Int(t.to_string()))
        }
    }

    /// Scan a JSON string. Returns (element, saw_json5_escapes).
    fn parse_string(&mut self) -> Result<(Jb, bool), usize> {
        if self.peek() != Some(b'"') {
            return Err(self.err());
        }
        self.pos += 1;
        let mut payload = String::new();
        let mut json5 = false;
        loop {
            let c = self.peek().ok_or(self.pos + 1)?;
            match c {
                b'"' => {
                    self.pos += 1;
                    let elem = if payload.contains('\\') {
                        if json5 {
                            Jb::Text5(payload)
                        } else {
                            Jb::TextJ(payload)
                        }
                    } else {
                        Jb::Text(payload)
                    };
                    return Ok((elem, json5));
                }
                b'\\' => {
                    self.pos += 1;
                    let e = self.peek().ok_or(self.pos + 1)?;
                    match e {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' | b'u' => {
                            payload.push('\\');
                            payload.push(e as char);
                            self.pos += 1;
                            if e == b'u' {
                                if self.peek() == Some(b'{') {
                                    // JSON5 \u{XX...} code point
                                    json5 = true;
                                    payload.push('{');
                                    self.pos += 1;
                                    while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                                        payload.push(self.peek().unwrap() as char);
                                        self.pos += 1;
                                    }
                                    if self.peek() != Some(b'}') {
                                        return Err(self.pos + 1);
                                    }
                                    payload.push('}');
                                    self.pos += 1;
                                } else {
                                    for _ in 0..4 {
                                        let h = self.peek().ok_or(self.pos + 1)?;
                                        if !h.is_ascii_hexdigit() {
                                            return Err(self.pos + 1);
                                        }
                                        payload.push(h as char);
                                        self.pos += 1;
                                    }
                                }
                            }
                        }
                        // JSON5 escapes
                        b'x' | b'v' | b'0'..=b'7' | b'\n' | b'\r' => {
                            json5 = true;
                            payload.push('\\');
                            payload.push(e as char);
                            self.pos += 1;
                            if e == b'x' {
                                for _ in 0..2 {
                                    let h = self.peek().ok_or(self.pos + 1)?;
                                    if !h.is_ascii_hexdigit() {
                                        return Err(self.pos + 1);
                                    }
                                    payload.push(h as char);
                                    self.pos += 1;
                                }
                            }
                        }
                        _ => return Err(self.pos + 1),
                    }
                }
                c if c < 0x20 => return Err(self.pos + 1),
                c if c < 0x80 => {
                    payload.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    // Multi-byte UTF-8: copy the whole sequence.
                    let rest =
                        std::str::from_utf8(&self.b[self.pos..]).map_err(|_| self.pos + 1)?;
                    let ch = rest.chars().next().ok_or(self.pos + 1)?;
                    payload.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Unquoted JSON5 object key: `{a:1}`.
    fn parse_ident_key(&mut self) -> Result<String, usize> {
        let start = self.pos;
        if self.peek() == Some(b'_') || self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.pos += 1;
            while matches!(
                self.peek(),
                Some(b'_') | Some(b'0'..=b'9') | Some(b'a'..=b'z') | Some(b'A'..=b'Z')
            ) {
                self.pos += 1;
            }
            Ok(std::str::from_utf8(&self.b[start..self.pos])
                .unwrap()
                .to_string())
        } else {
            Err(self.err())
        }
    }

    fn parse_array(&mut self) -> Result<Jb, usize> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Jb::Array(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    // Trailing comma (lenient only)
                    if self.peek() == Some(b']') && !self.strict {
                        self.pos += 1;
                        return Ok(Jb::Array(items));
                    }
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Jb::Array(items));
                }
                _ => return Err(self.pos + 1),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Jb, usize> {
        self.pos += 1; // {
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Jb::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = if self.peek() == Some(b'"') {
                self.parse_string()?.0
            } else if !self.strict {
                // Unquoted key or single-quoted key (JSON5)
                if self.peek() == Some(b'\'') {
                    // JSON5 single-quoted strings: treat as Text5-ish; rare.
                    self.pos += 1;
                    let mut s = String::new();
                    while let Some(c) = self.peek() {
                        if c == b'\'' {
                            break;
                        }
                        s.push(c as char);
                        self.pos += 1;
                    }
                    if self.peek() != Some(b'\'') {
                        return Err(self.pos + 1);
                    }
                    self.pos += 1;
                    Jb::Text5(s)
                } else {
                    Jb::Text(self.parse_ident_key()?)
                }
            } else {
                return Err(self.err());
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.pos + 1);
            }
            self.pos += 1;
            let v = self.parse_value()?;
            pairs.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                    if self.peek() == Some(b'}') && !self.strict {
                        self.pos += 1;
                        return Ok(Jb::Object(pairs));
                    }
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Jb::Object(pairs));
                }
                _ => return Err(self.pos + 1),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Escape decoding / rendering helpers
// ---------------------------------------------------------------------------

/// Decode JSON (and JSON5 when `json5`) escape sequences to the actual
/// string value.
pub fn decode_escapes(s: &str, json5: bool) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if i + 1 < b.len() => {
                i += 1;
                match b[i] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'v' => out.push('\u{000B}'),
                    b'0' => out.push('\0'),
                    b'u' => {
                        if json5 && i + 1 < b.len() && b[i + 1] == b'{' {
                            // \u{XX...}
                            let mut j = i + 2;
                            let mut v: u32 = 0;
                            while j < b.len() && b[j] != b'}' {
                                v = v * 16 + (b[j] as char).to_digit(16).unwrap_or(0);
                                j += 1;
                            }
                            if let Some(c) = char::from_u32(v) {
                                out.push(c);
                            }
                            i = j; // skip to '}' (loop ++ moves past)
                        } else {
                            let hex = s
                                .get(i + 1..i + 5)
                                .and_then(|h| u32::from_str_radix(h, 16).ok());
                            if let Some(cp) = hex {
                                if (0xD800..0xDC00).contains(&cp)
                                    && b.get(i + 5) == Some(&b'\\')
                                    && b.get(i + 6) == Some(&b'u')
                                {
                                    if let Some(lo) = s
                                        .get(i + 7..i + 11)
                                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                                    {
                                        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                        if let Some(ch) = char::from_u32(c) {
                                            out.push(ch);
                                        }
                                        i += 10;
                                    } else {
                                        out.push('\u{FFFD}');
                                        i += 4;
                                    }
                                } else if (0xDC00..0xE000).contains(&cp) {
                                    // Lone low surrogate
                                    out.push('\u{FFFD}');
                                } else {
                                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                    i += 4;
                                }
                            }
                        }
                    }
                    b'x' if json5 => {
                        if let Some(cp) = s
                            .get(i + 1..i + 3)
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                        {
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            i += 2;
                        }
                    }
                    _ => {
                        // Line continuation (backslash newline) or unknown:
                        if b[i] != b'\n' {
                            out.push('\\');
                            out.push(b[i] as char);
                        }
                    }
                }
                i += 1;
            }
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    out
}

/// Translate JSON5 escapes to RFC 8259 escapes (rendering a TEXT5
/// payload): decode to the value, re-escape minimally at render time.
fn translate_json5_escapes(s: &str) -> String {
    decode_escapes(s, true)
}

/// Write a raw string into JSON text, inserting RFC 8259 escapes
/// (SQLite's minimal set: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`,
/// `\u00xx` for other controls).
fn write_escaped_string(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// True when the string needs no escapes at all in JSON text — used to
/// classify SQL strings into TEXT vs TEXTJ/TEXTRAW.
pub fn json_safe(s: &str) -> bool {
    s.bytes().all(|c| c >= 0x20 && c != b'"' && c != b'\\')
}

// ---------------------------------------------------------------------------
// Canonicalization of non-canonical numeric text (rendering INT5/FLOAT5)
// ---------------------------------------------------------------------------

fn canonical_int5(s: &str) -> String {
    let t = s.trim_start_matches('+');
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        let neg = t.starts_with('-');
        let h = hex.strip_prefix('-').unwrap_or(hex);
        if let Ok(v) = u128::from_str_radix(h, 16) {
            let mut out = String::new();
            if neg {
                out.push('-');
            }
            out.push_str(&v.to_string());
            return out;
        }
    }
    t.to_string()
}

fn canonical_float5(s: &str) -> String {
    let t = s.trim_start_matches('+');
    if t == "Infinity" {
        return "9e999".to_string();
    }
    if t == "-Infinity" {
        return "-9e999".to_string();
    }
    if t == "NaN" {
        return "null".to_string();
    }
    // Fix ".5" → "0.5" and "1." → "1.0"; keep exponent text as-is.
    let (sign, rest) = if let Some(r) = t.strip_prefix('-') {
        ("-", r)
    } else {
        ("", t)
    };
    let mut out = String::new();
    out.push_str(sign);
    if let Some(frac) = rest.strip_prefix('.') {
        // ".5" / ".5e1": prepend the leading zero (keep the dot).
        out.push('0');
        out.push('.');
        out.push_str(frac);
    } else if let Some(pos) = rest.find('.') {
        if rest[pos + 1..].is_empty() {
            // "1." → "1.0"
            out.push_str(rest);
            out.push('0');
        } else {
            out.push_str(rest);
        }
    } else {
        out.push_str(rest);
    }
    out
}

// ---------------------------------------------------------------------------
// SQL REAL rendering — SQLite's `%!.15g` with a guaranteed decimal point
// ---------------------------------------------------------------------------

/// Render an f64 exactly like SQLite's JSON functions do:
/// * 0.0 → "0.0", -0.0 → "0.0"
/// * 15 significant digits, `%g` fixed/scientific selection
///   (scientific when exponent < -4 or >= 15)
/// * always a '.' in the output ("100" → "100.0", "1e+15" → "1.0e+15")
/// * ±inf → "9.0e+999" / "-9.0e+999"
pub fn render_sql_f64(v: f64) -> String {
    if v.is_nan() {
        return "null".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "9.0e+999" } else { "-9.0e+999" }.to_string();
    }
    crate::types::format_real_sig(v, 15)
}

// ---------------------------------------------------------------------------
// JSONB binary codec — encode (shortest headers) / decode (structural)
// ---------------------------------------------------------------------------

/// Append the element header for `typ` with a payload of `len` bytes.
/// Shortest form: high nibble 0-11 = inline size; 12/13/14/15 = 1/2/4/8
/// big-endian size bytes.
fn push_header(out: &mut Vec<u8>, typ: u8, len: usize) {
    if len <= 11 {
        out.push(((len as u8) << 4) | typ);
    } else if len <= 0xff {
        out.push(0xc0 | typ);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0xd0 | typ);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else if len <= 0xffff_ffff {
        out.push(0xe0 | typ);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        out.push(0xf0 | typ);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
}

fn encode_elem(out: &mut Vec<u8>, jb: &Jb) {
    match jb {
        Jb::Null => out.push(T_NULL),
        Jb::True => out.push(T_TRUE),
        Jb::False => out.push(T_FALSE),
        Jb::Int(s) => push_header(out, T_INT, s.len()),
        Jb::Int5(s) => push_header(out, T_INT5, s.len()),
        Jb::Float(s) => push_header(out, T_FLOAT, s.len()),
        Jb::Float5(s) => push_header(out, T_FLOAT5, s.len()),
        Jb::Text(s) => push_header(out, T_TEXT, s.len()),
        Jb::TextJ(s) => push_header(out, T_TEXTJ, s.len()),
        Jb::Text5(s) => push_header(out, T_TEXT5, s.len()),
        Jb::TextRaw(s) => push_header(out, T_TEXTRAW, s.len()),
        Jb::Array(items) => {
            let mut payload = Vec::new();
            for item in items {
                encode_elem(&mut payload, item);
            }
            push_header(out, T_ARRAY, payload.len());
            out.extend_from_slice(&payload);
        }
        Jb::Object(pairs) => {
            let mut payload = Vec::new();
            for (k, v) in pairs {
                encode_elem(&mut payload, k);
                encode_elem(&mut payload, v);
            }
            push_header(out, T_OBJECT, payload.len());
            out.extend_from_slice(&payload);
        }
    }
    match jb {
        Jb::Int(s)
        | Jb::Int5(s)
        | Jb::Float(s)
        | Jb::Float5(s)
        | Jb::Text(s)
        | Jb::TextJ(s)
        | Jb::Text5(s)
        | Jb::TextRaw(s) => out.extend_from_slice(s.as_bytes()),
        _ => {}
    }
}

/// Encode to JSONB bytes — byte-identical to SQLite's `jsonb()`.
pub fn jb_to_bytes(jb: &Jb) -> Vec<u8> {
    let mut out = Vec::new();
    encode_elem(&mut out, jb);
    out
}

/// Append a container element (header + payload) — used by the
/// jsonb_group_array / jsonb_group_object aggregate finalizers to wrap
/// accumulated element fragments.
pub(crate) fn push_container(out: &mut Vec<u8>, typ: u8, payload: &[u8]) {
    push_header(out, typ, payload.len());
    out.extend_from_slice(payload);
}

/// Encode one element into `out` (the `__jsonb_object_frag` accumulator).
pub(crate) fn encode_elem_pub(out: &mut Vec<u8>, jb: &Jb) {
    encode_elem(out, jb);
}

/// Decode header at `pos`. Returns (typ, payload_len, header_len).
fn read_header(b: &[u8], pos: usize) -> Option<(u8, usize, usize)> {
    let h = *b.get(pos)?;
    let typ = h & 0x0f;
    let hn = (h >> 4) as usize;
    if hn <= 11 {
        Some((typ, hn, 1))
    } else {
        let n = match hn {
            12 => 1,
            13 => 2,
            14 => 4,
            15 => 8,
            _ => return None,
        };
        if pos + 1 + n > b.len() {
            return None;
        }
        let mut len: usize = 0;
        for i in 0..n {
            len = (len << 8) | b[pos + 1 + i] as usize;
        }
        Some((typ, len, 1 + n))
    }
}

/// Decode one element at `pos`; advances `pos` past the element.
/// Structurally validates (payload bounds, container fill, object key
/// types) exactly like SQLite's JSONB acceptance rules.
fn decode_elem(b: &[u8], pos: &mut usize) -> Result<Jb, ()> {
    let (typ, len, hdr) = read_header(b, *pos).ok_or(())?;
    let payload_start = *pos + hdr;
    let payload_end = payload_start.checked_add(len).ok_or(())?;
    if payload_end > b.len() {
        return Err(());
    }
    *pos = payload_end;
    match typ {
        T_NULL => {
            if len != 0 {
                return Err(());
            }
            Ok(Jb::Null)
        }
        T_TRUE => {
            if len != 0 {
                return Err(());
            }
            Ok(Jb::True)
        }
        T_FALSE => {
            if len != 0 {
                return Err(());
            }
            Ok(Jb::False)
        }
        T_INT | T_INT5 | T_FLOAT | T_FLOAT5 | T_TEXT | T_TEXTJ | T_TEXT5 | T_TEXTRAW => {
            let s = std::str::from_utf8(&b[payload_start..payload_end])
                .map_err(|_| ())?
                .to_string();
            Ok(match typ {
                T_INT => Jb::Int(s),
                T_INT5 => Jb::Int5(s),
                T_FLOAT => Jb::Float(s),
                T_FLOAT5 => Jb::Float5(s),
                T_TEXT => Jb::Text(s),
                T_TEXTJ => Jb::TextJ(s),
                T_TEXT5 => Jb::Text5(s),
                _ => Jb::TextRaw(s),
            })
        }
        T_ARRAY => {
            let mut items = Vec::new();
            let mut p = payload_start;
            while p < payload_end {
                items.push(decode_elem(b, &mut p)?);
            }
            if p != payload_end {
                return Err(());
            }
            Ok(Jb::Array(items))
        }
        T_OBJECT => {
            let mut pairs = Vec::new();
            let mut p = payload_start;
            while p < payload_end {
                let key = decode_elem(b, &mut p)?;
                if !matches!(
                    key,
                    Jb::Text(_) | Jb::TextJ(_) | Jb::Text5(_) | Jb::TextRaw(_)
                ) {
                    return Err(());
                }
                if p >= payload_end {
                    return Err(());
                }
                let val = decode_elem(b, &mut p)?;
                pairs.push((key, val));
            }
            if p != payload_end {
                return Err(());
            }
            Ok(Jb::Object(pairs))
        }
        _ => Err(()),
    }
}

/// Decode a JSONB blob to the raw tree. The root element must exactly
/// fill the blob (SQLite's acceptance rule). The unit error is
/// deliberate: callers only need the valid/invalid distinction.
#[allow(clippy::result_unit_err)]
pub fn jb_from_bytes(b: &[u8]) -> Result<Jb, ()> {
    if b.is_empty() {
        return Err(());
    }
    let mut pos = 0usize;
    let jb = decode_elem(b, &mut pos)?;
    if pos != b.len() {
        return Err(());
    }
    Ok(jb)
}

/// Is this blob a valid JSONB value?
pub fn is_valid_jsonb(b: &[u8]) -> bool {
    jb_from_bytes(b).is_ok()
}

// ---------------------------------------------------------------------------
// Spanned decode — element byte offsets for json_each / json_tree ids
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SpNode {
    /// Scalar elements (types 0-10) carry their Jb directly.
    Scalar(Jb),
    Array(Vec<Sp>),
    Object(Vec<(Sp, Sp)>),
}

/// One decoded element with its start offset (the `id` column of
/// `json_each`/`json_tree`: byte offsets into the canonical JSONB).
#[derive(Debug)]
pub struct Sp {
    pub off: usize,
    pub node: SpNode,
}

impl Sp {
    /// The element subtree as a raw tree (assembled on demand).
    pub fn to_jb(&self) -> Jb {
        match &self.node {
            SpNode::Scalar(jb) => jb.clone(),
            SpNode::Array(items) => Jb::Array(items.iter().map(|s| s.to_jb()).collect()),
            SpNode::Object(pairs) => {
                Jb::Object(pairs.iter().map(|(k, v)| (k.to_jb(), v.to_jb())).collect())
            }
        }
    }

    pub fn type_name(&self) -> &'static str {
        match &self.node {
            SpNode::Scalar(jb) => jb.type_name(),
            SpNode::Array(_) => "array",
            SpNode::Object(_) => "object",
        }
    }

    /// SQL value of this element (`value`/`atom` columns of json_each).
    pub fn to_value(&self) -> Value {
        match &self.node {
            SpNode::Scalar(jb) => jb.to_value(),
            SpNode::Array(_) | SpNode::Object(_) => Value::Text(self.to_jb().render().into()),
        }
    }
}

fn decode_sp_elem(b: &[u8], pos: &mut usize) -> Result<Sp, ()> {
    let start = *pos;
    let (typ, len, hdr) = read_header(b, *pos).ok_or(())?;
    let payload_start = *pos + hdr;
    let payload_end = payload_start.checked_add(len).ok_or(())?;
    if payload_end > b.len() {
        return Err(());
    }
    *pos = payload_end;
    match typ {
        T_NULL => {
            if len != 0 {
                return Err(());
            }
            Ok(Sp {
                off: start,
                node: SpNode::Scalar(Jb::Null),
            })
        }
        T_TRUE => {
            if len != 0 {
                return Err(());
            }
            Ok(Sp {
                off: start,
                node: SpNode::Scalar(Jb::True),
            })
        }
        T_FALSE => {
            if len != 0 {
                return Err(());
            }
            Ok(Sp {
                off: start,
                node: SpNode::Scalar(Jb::False),
            })
        }
        T_INT | T_INT5 | T_FLOAT | T_FLOAT5 | T_TEXT | T_TEXTJ | T_TEXT5 | T_TEXTRAW => {
            let s = std::str::from_utf8(&b[payload_start..payload_end])
                .map_err(|_| ())?
                .to_string();
            let jb = match typ {
                T_INT => Jb::Int(s),
                T_INT5 => Jb::Int5(s),
                T_FLOAT => Jb::Float(s),
                T_FLOAT5 => Jb::Float5(s),
                T_TEXT => Jb::Text(s),
                T_TEXTJ => Jb::TextJ(s),
                T_TEXT5 => Jb::Text5(s),
                _ => Jb::TextRaw(s),
            };
            Ok(Sp {
                off: start,
                node: SpNode::Scalar(jb),
            })
        }
        T_ARRAY => {
            let mut items = Vec::new();
            let mut p = payload_start;
            while p < payload_end {
                items.push(decode_sp_elem(b, &mut p)?);
            }
            if p != payload_end {
                return Err(());
            }
            Ok(Sp {
                off: start,
                node: SpNode::Array(items),
            })
        }
        T_OBJECT => {
            let mut pairs = Vec::new();
            let mut p = payload_start;
            while p < payload_end {
                let key = decode_sp_elem(b, &mut p)?;
                if !matches!(key.node, SpNode::Scalar(ref jb) if jb.is_string()) {
                    return Err(());
                }
                if p >= payload_end {
                    return Err(());
                }
                let val = decode_sp_elem(b, &mut p)?;
                pairs.push((key, val));
            }
            if p != payload_end {
                return Err(());
            }
            Ok(Sp {
                off: start,
                node: SpNode::Object(pairs),
            })
        }
        _ => Err(()),
    }
}

/// Decode a JSONB blob to the spanned tree (root must exactly fill).
#[allow(clippy::result_unit_err)]
pub fn sp_from_bytes(b: &[u8]) -> Result<Sp, ()> {
    if b.is_empty() {
        return Err(());
    }
    let mut pos = 0usize;
    let sp = decode_sp_elem(b, &mut pos)?;
    if pos != b.len() {
        return Err(());
    }
    Ok(sp)
}

// ---------------------------------------------------------------------------
// SQL value bridge
// ---------------------------------------------------------------------------

/// SQL value → element, exactly like SQLite's json functions building
/// documents from arguments:
/// * INTEGER → INT; REAL → FLOAT with SQLite's `%!.15g` text
/// * TEXT → TEXT (safe) / TEXTJ (escapes inserted into the payload)
/// * BLOB → embedded element when it is valid JSON/JSONB, else null
/// * NULL → null; NaN → null
pub fn value_to_jb(v: &Value) -> Jb {
    match v {
        Value::Null => Jb::Null,
        Value::Integer(i) => Jb::Int(i.to_string()),
        Value::Real(r) => {
            if r.is_nan() {
                Jb::Null
            } else {
                Jb::Float(render_sql_f64(*r))
            }
        }
        Value::Text(t) => {
            let s: &str = t;
            if json_safe(s) {
                Jb::Text(s.to_string())
            } else {
                // Escapes inserted eagerly — SQLite's TEXTJ from SQL values.
                let mut p = String::with_capacity(s.len() + 8);
                write_escaped_string(s, &mut p);
                Jb::TextJ(p)
            }
        }
        Value::Blob(b) => match blob_to_jb(b) {
            Ok(jb) => jb,
            Err(()) => Jb::Null,
        },
    }
}

/// Blob → element: JSONB decode first (a well-formed element that exactly
/// fills the blob IS JSONB — sqlite.org/jsonb.html §3.4), then a JSON-text
/// parse of the bytes.
#[allow(clippy::result_unit_err)]
pub fn blob_to_jb(b: &[u8]) -> Result<Jb, ()> {
    if let Ok(jb) = jb_from_bytes(b) {
        return Ok(jb);
    }
    if let Ok(s) = std::str::from_utf8(b) {
        if let Ok(jb) = parse_lenient(s) {
            return Ok(jb);
        }
    }
    Err(())
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve `path` against the tree (first matching key wins, matching
/// SQLite's duplicate-key lookup semantics).
pub fn jb_resolve<'a>(jb: &'a Jb, segs: &[crate::executor::json::PathSeg]) -> Option<&'a Jb> {
    let mut cur = jb;
    for seg in segs {
        cur = match (seg, cur) {
            (crate::executor::json::PathSeg::Key(k), Jb::Object(pairs)) => pairs
                .iter()
                .find(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                .map(|(_, v)| v)?,
            (crate::executor::json::PathSeg::Index(i), Jb::Array(items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx < 0 || idx >= items.len() as i64 {
                    return None;
                }
                &items[idx as usize]
            }
            _ => return None,
        };
    }
    Some(cur)
}

// ---------------------------------------------------------------------------
// Mutation functions (json_set / json_insert / json_replace / json_remove)
// operating on the raw tree — untouched subtrees keep their original text.
// ---------------------------------------------------------------------------

use crate::executor::json::PathSeg;

/// Set/insert/replace the value at `segs` inside `root`. Mirrors SQLite:
/// duplicate keys match the FIRST occurrence; created keys are TEXTRAW
/// elements (SQLite's path-origin key encoding).
pub fn jb_set_at(root: Jb, segs: &[PathSeg], new_val: Jb, create: bool, insert_only: bool) -> Jb {
    fn walk(node: Jb, segs: &[PathSeg], new_val: &Jb, create: bool, insert_only: bool) -> Jb {
        if segs.is_empty() {
            if insert_only {
                return node;
            }
            return new_val.clone();
        }
        match (&segs[0], node) {
            (PathSeg::Key(k), Jb::Object(mut members)) => {
                if let Some(pos) = members
                    .iter()
                    .position(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                {
                    let child = std::mem::replace(&mut members[pos].1, Jb::Null);
                    members[pos].1 = walk(child, &segs[1..], new_val, create, insert_only);
                    Jb::Object(members)
                } else if create {
                    let key = Jb::TextRaw(k.clone());
                    let child = walk(
                        Jb::Object(Vec::new()),
                        &segs[1..],
                        new_val,
                        create,
                        insert_only,
                    );
                    members.push((key, child));
                    Jb::Object(members)
                } else {
                    Jb::Object(members)
                }
            }
            (PathSeg::Index(i), Jb::Array(mut items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx >= 0 && (idx as usize) < items.len() {
                    let pos = idx as usize;
                    let child = std::mem::replace(&mut items[pos], Jb::Null);
                    items[pos] = walk(child, &segs[1..], new_val, create, insert_only);
                } else if create && idx >= 0 && (idx as usize) == items.len() {
                    // Append at the end (SQLite creates only the slot
                    // exactly one past the last element).
                    let child = walk(
                        Jb::Object(Vec::new()),
                        &segs[1..],
                        new_val,
                        create,
                        insert_only,
                    );
                    items.push(child);
                }
                Jb::Array(items)
            }
            (_, node) => node,
        }
    }
    walk(root, segs, &new_val, create, insert_only)
}

/// Remove the value at `segs` — FIRST matching occurrence only
/// (SQLite: `json_remove('{"a":1,"a":2}','$.a')` → `{"a":2}`).
/// Empty segs (the root path `$`) removes the whole document.
pub fn jb_remove_at(root: Jb, segs: &[PathSeg]) -> Option<Jb> {
    fn walk(node: Jb, segs: &[PathSeg]) -> Option<Jb> {
        if segs.is_empty() {
            // Removing the root: caller maps this to SQL NULL.
            return None;
        }
        if segs.len() == 1 {
            return Some(match (&segs[0], node) {
                (PathSeg::Key(k), Jb::Object(mut members)) => {
                    if let Some(pos) = members
                        .iter()
                        .position(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                    {
                        members.remove(pos);
                    }
                    Jb::Object(members)
                }
                (PathSeg::Index(i), Jb::Array(mut items)) => {
                    let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                    if idx >= 0 && (idx as usize) < items.len() {
                        items.remove(idx as usize);
                    }
                    Jb::Array(items)
                }
                (_, node) => node,
            });
        }
        Some(match (&segs[0], node) {
            (PathSeg::Key(k), Jb::Object(mut members)) => {
                if let Some(pos) = members
                    .iter()
                    .position(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                {
                    let child = std::mem::replace(&mut members[pos].1, Jb::Null);
                    if let Some(new_child) = walk(child, &segs[1..]) {
                        members[pos].1 = new_child;
                    } else {
                        members.remove(pos);
                    }
                }
                Jb::Object(members)
            }
            (PathSeg::Index(i), Jb::Array(mut items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx >= 0 && (idx as usize) < items.len() {
                    let pos = idx as usize;
                    let child = std::mem::replace(&mut items[pos], Jb::Null);
                    if let Some(new_child) = walk(child, &segs[1..]) {
                        items[pos] = new_child;
                    } else {
                        items.remove(pos);
                    }
                }
                Jb::Array(items)
            }
            (_, node) => node,
        })
    }
    walk(root, segs)
}

/// RFC 7396 JSON Merge Patch on the raw tree — untouched members keep
/// their original text; the patch's key and value elements are carried
/// over verbatim.
pub fn jb_patch(target: &Jb, patch: &Jb) -> Jb {
    match patch {
        Jb::Object(patch_pairs) => {
            let mut out: Vec<(Jb, Jb)> = match target {
                Jb::Object(m) => m.clone(),
                _ => Vec::new(),
            };
            for (pk, pv) in patch_pairs {
                let k = pk.string_value().unwrap_or_default();
                if matches!(pv, Jb::Null) {
                    out.retain(|(mk, _)| mk.string_value().as_deref() != Some(k.as_str()));
                } else {
                    let existing = out
                        .iter()
                        .find(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                        .map(|(_, v)| v.clone());
                    let merged = match existing {
                        Some(e) => jb_patch(&e, pv),
                        None => jb_patch(&Jb::Object(Vec::new()), pv),
                    };
                    if let Some(pos) = out
                        .iter()
                        .position(|(mk, _)| mk.string_value().as_deref() == Some(k.as_str()))
                    {
                        out[pos].1 = merged;
                    } else {
                        out.push((pk.clone(), merged));
                    }
                }
            }
            Jb::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// json_each / json_tree row generation over the spanned tree
// ---------------------------------------------------------------------------

pub struct EachRow {
    /// Integer for array elements, Text for object members, Null for the
    /// root / start row.
    pub key: Value,
    /// SQL value (scalars) or the minified JSON text (containers).
    pub value: Value,
    pub type_name: &'static str,
    /// SQL value for scalars, NULL for containers.
    pub atom: Value,
    /// Byte offset of the element in the canonical JSONB encoding —
    /// SQLite's `id` column.
    pub id: i64,
    /// `None` renders as SQL NULL.
    pub parent: Option<i64>,
    pub fullkey: String,
    pub path: String,
}

/// Resolve a path against the spanned tree, returning the target node,
/// its contextual key, and its fullkey / container fullkey.
pub struct SpResolved<'a> {
    pub node: &'a Sp,
    /// Key under which the target sits (None at the document root).
    pub key: Option<Value>,
    /// Offset of the target's KEY element when it is an object member —
    /// SQLite's row id for member rows (the key position, not the value
    /// position).
    pub key_off: Option<usize>,
    /// Fullkey of the target's CONTAINER (the `path` column of the start row).
    pub container_fullkey: String,
    /// Fullkey of the target itself.
    pub target_fullkey: String,
    pub is_root: bool,
}

pub fn sp_resolve<'a>(root: &'a Sp, segs: &[PathSeg]) -> Option<SpResolved<'a>> {
    let mut cur = root;
    let mut key: Option<Value> = None;
    let mut key_off: Option<usize> = None;
    // `container` = fullkey of the node `cur` points at (the container of
    // the NEXT step's target). `final_container` = container of the final
    // target (snapshot taken before each step).
    let mut container = "$".to_string();
    let mut final_container = "$".to_string();
    let mut target_fullkey = "$".to_string();
    let mut is_root = true;
    for seg in segs {
        final_container = container.clone();
        match (seg, &cur.node) {
            (PathSeg::Key(k), SpNode::Object(pairs)) => {
                let found = pairs.iter().find(|(mk, _)| {
                    matches!(&mk.node, SpNode::Scalar(jb) if jb.string_value().as_deref() == Some(k.as_str()))
                });
                let (mk, v) = found?;
                key = Some(Value::Text(k.as_str().into()));
                key_off = Some(mk.off);
                target_fullkey = format!("{}.{}", container, escape_path_key(k));
                container = target_fullkey.clone();
                cur = v;
                is_root = false;
            }
            (PathSeg::Index(i), SpNode::Array(items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx < 0 || idx >= items.len() as i64 {
                    return None;
                }
                key = Some(Value::Integer(idx));
                key_off = None;
                target_fullkey = format!("{}[{}]", container, idx);
                container = target_fullkey.clone();
                cur = &items[idx as usize];
                is_root = false;
            }
            _ => return None,
        }
    }
    Some(SpResolved {
        node: cur,
        key,
        key_off,
        container_fullkey: final_container,
        target_fullkey,
        is_root,
    })
}

/// Quote an object key for a path string (SQLite quotes keys with
/// non-identifier characters: `$."a b"`).
fn escape_path_key(k: &str) -> String {
    if !k.is_empty() && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        k.to_string()
    } else {
        format!("\"{}\"", k.replace('"', "\\\""))
    }
}

/// Emit the walk rows for `json_each` (one level) / `json_tree`
/// (recursive) starting at `start`.
///
/// * `json_each`: container → its children (parent = NULL for the top
///   rows); scalar → the row for the scalar itself.
/// * `json_tree`: the start row itself first (parent = NULL, the
///   contextual key), then all descendants (parent = the id of the row
///   that owns them).
pub fn each_rows(start: &SpResolved<'_>, recursive: bool) -> Vec<EachRow> {
    let mut out = Vec::new();
    let start_fullkey = start.target_fullkey.clone();
    // The start row's id: the KEY element's offset for object members,
    // the element offset otherwise (SQLite's row-id rule).
    let start_row_id = start.key_off.unwrap_or(start.node.off) as i64;
    if recursive {
        // json_tree: the start row itself.
        let start_key = start.key.clone().unwrap_or(Value::Null);
        push_row(
            &start_key,
            start.node,
            start_row_id,
            &start_fullkey,
            None,
            &start.container_fullkey,
            &mut out,
        );
    }
    let top_parent = if recursive { Some(start_row_id) } else { None };
    match &start.node.node {
        SpNode::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let fullkey = format!("{}[{}]", start_fullkey, i);
                push_row(
                    &Value::Integer(i as i64),
                    item,
                    item.off as i64,
                    &fullkey,
                    top_parent,
                    &start_fullkey,
                    &mut out,
                );
                if recursive {
                    walk_sp(item, &fullkey, item.off as i64, &fullkey, &mut out);
                }
            }
        }
        SpNode::Object(pairs) => {
            for (ksp, vsp) in pairs.iter() {
                let k = key_string(&ksp.node);
                let fullkey = format!("{}.{}", start_fullkey, escape_path_key(&k));
                // The member row's id is the KEY element's offset (SQLite
                // reports the pair's key position, not the value's).
                let row_id = ksp.off as i64;
                push_row(
                    &Value::Text(k.as_str().into()),
                    vsp,
                    row_id,
                    &fullkey,
                    top_parent,
                    &start_fullkey,
                    &mut out,
                );
                if recursive {
                    walk_sp(vsp, &fullkey, row_id, &fullkey, &mut out);
                }
            }
        }
        SpNode::Scalar(_) => {
            if !recursive {
                // json_each of a scalar document: the row for the scalar
                // itself (json_each(5) → one row, key NULL).
                push_row(
                    &Value::Null,
                    start.node,
                    start.node.off as i64,
                    &start_fullkey,
                    None,
                    &start_fullkey,
                    &mut out,
                );
            }
        }
    }
    out
}

/// json_tree descent: emit the node itself, then recurse.
fn walk_sp(node: &Sp, fullkey: &str, parent: i64, container_path: &str, out: &mut Vec<EachRow>) {
    match &node.node {
        SpNode::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let fk = format!("{}[{}]", fullkey, i);
                let row_id = item.off as i64;
                push_row(
                    &Value::Integer(i as i64),
                    item,
                    row_id,
                    &fk,
                    Some(parent),
                    container_path,
                    out,
                );
                walk_sp(item, &fk, row_id, &fk, out);
            }
        }
        SpNode::Object(pairs) => {
            for (ksp, vsp) in pairs.iter() {
                let k = key_string(&ksp.node);
                let fk = format!("{}.{}", fullkey, escape_path_key(&k));
                let row_id = ksp.off as i64;
                push_row(
                    &Value::Text(k.as_str().into()),
                    vsp,
                    row_id,
                    &fk,
                    Some(parent),
                    container_path,
                    out,
                );
                walk_sp(vsp, &fk, row_id, &fk, out);
            }
        }
        SpNode::Scalar(_) => {}
    }
}

fn key_string(node: &SpNode) -> String {
    match node {
        SpNode::Scalar(jb) => jb.string_value().unwrap_or_default(),
        _ => String::new(),
    }
}

fn push_row(
    key: &Value,
    node: &Sp,
    id: i64,
    fullkey: &str,
    parent: Option<i64>,
    path: &str,
    out: &mut Vec<EachRow>,
) {
    let value = node.to_value();
    let atom = match &node.node {
        SpNode::Scalar(_) => value.clone(),
        _ => Value::Null,
    };
    out.push(EachRow {
        key: key.clone(),
        value,
        type_name: node.type_name(),
        atom,
        id,
        parent,
        fullkey: fullkey.to_string(),
        path: path.to_string(),
    });
}

// ---------------------------------------------------------------------------
// -> / ->> right-hand sides
// ---------------------------------------------------------------------------

/// Convert the RHS of `->` / `->>` into path segments:
/// * INTEGER → array index (negative counts from the end)
/// * TEXT starting with `$` → a full path (parse errors propagate)
/// * TEXT `[N]` → array index (non-negative; anything else in brackets
///   is a bad path — SQLite: `'[10,20]' -> '[-1]'` errors)
/// * other nonempty TEXT → a single object key (SQLite: `x -> 'a.b'`
///   looks up the literal key "a.b", NOT a nested path)
/// * empty TEXT → bad JSON path
pub fn arrow_rhs_segments(v: &Value) -> Result<Option<Vec<PathSeg>>, String> {
    match v {
        Value::Null => Ok(None),
        Value::Integer(i) => Ok(Some(vec![PathSeg::Index(*i)])),
        Value::Text(t) => {
            let s: &str = t;
            if s.is_empty() {
                return Err(format!("bad JSON path: '{}'", s));
            }
            if s.starts_with('$') {
                match crate::executor::json::parse_path(s) {
                    Some(p) => Ok(Some(p.segs().to_vec())),
                    None => Err(format!("bad JSON path: '{}'", s)),
                }
            } else if s.starts_with('[') && s.ends_with(']') {
                match s[1..s.len() - 1].parse::<i64>() {
                    Ok(n) if n >= 0 => Ok(Some(vec![PathSeg::Index(n)])),
                    _ => Err(format!("bad JSON path: '{}'", s)),
                }
            } else {
                Ok(Some(vec![PathSeg::Key(s.to_string())]))
            }
        }
        Value::Real(r) => Ok(Some(vec![PathSeg::Index(*r as i64)])),
        Value::Blob(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_render_matches_sqlite() {
        assert_eq!(render_sql_f64(1.0), "1.0");
        assert_eq!(render_sql_f64(0.0), "0.0");
        assert_eq!(render_sql_f64(-0.0), "0.0");
        assert_eq!(render_sql_f64(1.5), "1.5");
        assert_eq!(render_sql_f64(1e300), "1.0e+300");
        assert_eq!(render_sql_f64(1e15), "1.0e+15");
        assert_eq!(render_sql_f64(1e10), "10000000000.0");
        assert_eq!(render_sql_f64(1e-5), "1.0e-05");
        assert_eq!(render_sql_f64(2e-4), "0.0002");
        assert_eq!(render_sql_f64(123456789.12345678), "123456789.123457");
        assert_eq!(render_sql_f64(123456789012345678.0), "1.23456789012346e+17");
        assert_eq!(render_sql_f64(f64::INFINITY), "9.0e+999");
        assert_eq!(render_sql_f64(0.1), "0.1");
        assert_eq!(render_sql_f64(2.5), "2.5");
    }

    #[test]
    fn parse_classification() {
        assert_eq!(parse_lenient("1e2").unwrap(), Jb::Float("1e2".into()));
        assert_eq!(
            parse_lenient("[+1]").unwrap(),
            Jb::Array(vec![Jb::Int("1".into())])
        );
        assert_eq!(
            parse_lenient("[.5]").unwrap(),
            Jb::Array(vec![Jb::Float5(".5".into())])
        );
        assert_eq!(
            parse_lenient("[1.]").unwrap(),
            Jb::Array(vec![Jb::Float5("1.".into())])
        );
        assert_eq!(
            parse_lenient("[0x10]").unwrap(),
            Jb::Array(vec![Jb::Int5("0x10".into())])
        );
        assert_eq!(
            parse_lenient("[Infinity]").unwrap(),
            Jb::Array(vec![Jb::Float("9e999".into())])
        );
        assert_eq!(parse_lenient("[NaN]").unwrap(), Jb::Array(vec![Jb::Null]));
        assert!(parse_lenient("[00]").is_err());
        assert!(parse_lenient("[1,]").is_ok());
        assert_eq!(parse_lenient("{\"a\":1,}").unwrap().render(), "{\"a\":1}");
        assert_eq!(parse_lenient("{a:1}").unwrap().render(), "{\"a\":1}");
        // Raw preservation
        assert_eq!(parse_lenient("[1.50]").unwrap().render(), "[1.50]");
        assert_eq!(
            parse_lenient("\"a\\u0062c\"").unwrap().render(),
            "\"a\\u0062c\""
        );
    }
}
