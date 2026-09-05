//! JSON1 support: a small self-contained JSON engine (parser, path
//! evaluation, serialization) and the SQLite JSON1 scalar functions.
//!
//! Supported functions: `json`, `json_extract`, `json_valid`, `json_type`,
//! `json_quote`, `json_array`, `json_object`, `json_array_length`,
//! `json_insert`, `json_replace`, `json_set`, `json_remove`, `json_patch`.
//! Path syntax: `$`, `.key`, `."quoted key"`, `[index]`, `[#-n]` (negative
//! index from the end), and chained forms — the common subset of SQLite's
//! path language.

use crate::types::Value;

// ---------------------------------------------------------------------------
// JSON tree
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    True,
    False,
    Integer(i64),
    Real(f64),
    Text(String),
    /// A JSON string value (distinct from Text-as-SQL-value: JSON strings
    /// come from the document; `json_extract` unquotes them).
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::True | Json::False => "true",
            Json::Integer(_) => "integer",
            Json::Real(_) => "real",
            Json::Str(_) | Json::Text(_) => "text",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct JsonParser<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Json::Str),
            b't' => self.parse_lit("true", Json::True),
            b'f' => self.parse_lit("false", Json::False),
            b'n' => self.parse_lit("null", Json::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_lit(&mut self, lit: &str, v: Json) -> Option<Json> {
        if self.b[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Some(v)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let mut is_real = false;
        if self.peek() == Some(b'.') {
            is_real = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_real = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.pos]).ok()?;
        if is_real {
            s.parse::<f64>().ok().map(Json::Real)
        } else {
            match s.parse::<i64>() {
                Ok(i) => Some(Json::Integer(i)),
                Err(_) => s.parse::<f64>().ok().map(Json::Real),
            }
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.peek() != Some(b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek()? {
                b'"' => {
                    self.pos += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.pos += 1;
                    match self.peek()? {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            // \uXXXX (surrogate pairs handled)
                            let hex = self.b.get(self.pos + 1..self.pos + 5)?;
                            let cp =
                                u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                            self.pos += 4;
                            if (0xD800..0xDC00).contains(&cp) {
                                // expect low surrogate
                                if self.b.get(self.pos + 1) == Some(&b'\\')
                                    && self.b.get(self.pos + 2) == Some(&b'u')
                                {
                                    let hex2 = self.b.get(self.pos + 3..self.pos + 7)?;
                                    let lo =
                                        u32::from_str_radix(std::str::from_utf8(hex2).ok()?, 16)
                                            .ok()?;
                                    self.pos += 6;
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c)?);
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else {
                                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return None,
                    }
                    self.pos += 1;
                }
                c if c < 0x80 => {
                    out.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    // Multi-byte UTF-8: copy the whole sequence.
                    let rest = std::str::from_utf8(&self.b[self.pos..]).ok()?;
                    let ch = rest.chars().next()?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn parse_array(&mut self) -> Option<Json> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_object(&mut self) -> Option<Json> {
        self.pos += 1; // {
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Json::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return None;
            }
            self.pos += 1;
            let v = self.parse_value()?;
            members.push((key, v));
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(members));
                }
                _ => return None,
            }
        }
    }
}

/// Parse a complete JSON document (trailing non-whitespace is invalid,
/// matching SQLite's json_valid).
pub fn parse_json(s: &str) -> Option<Json> {
    let mut p = JsonParser::new(s);
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos == p.b.len() {
        Some(v)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Serializer (minified, SQLite-compatible)
// ---------------------------------------------------------------------------

pub fn json_to_string(j: &Json) -> String {
    let mut out = String::new();
    write_json(j, &mut out);
    out
}

fn write_json(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::True => out.push_str("true"),
        Json::False => out.push_str("false"),
        Json::Integer(i) => out.push_str(&i.to_string()),
        Json::Real(r) => {
            if r.is_finite() {
                if *r == r.trunc() && r.abs() < 1e15 {
                    // SQLite prints integral reals with .0
                    out.push_str(&format!("{:.1}", r));
                } else {
                    out.push_str(&format!("{}", r));
                }
            } else {
                out.push_str("9e999");
            }
        }
        Json::Str(s) | Json::Text(s) => write_json_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_json(v, out);
            }
            out.push('}');
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Path evaluation: $ .key ."quoted key" [index] [#-n]
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum PathSeg {
    Key(String),
    Index(i64), // negative = from end
}

#[derive(Clone, Debug)]
pub struct JsonPath {
    segs: Vec<PathSeg>,
}

/// Parse a JSON path like `$`, `$.a.b`, `$[0].x`, `."q k"`, `[#-1]`.
/// Returns None on malformed paths.
pub fn parse_path(p: &str) -> Option<JsonPath> {
    let b = p.as_bytes();
    let mut segs = Vec::new();
    if b.first() != Some(&b'$') {
        return None;
    }
    // Position 0 is the '$' root marker; the path body starts after it.
    let mut i = 1usize;
    while i < b.len() {
        match b[i] {
            b'.' => {
                i += 1;
                if i < b.len() && b[i] == b'"' {
                    // quoted key
                    let mut j = i + 1;
                    let mut key = String::new();
                    while j < b.len() && b[j] != b'"' {
                        if b[j] == b'\\' && j + 1 < b.len() {
                            key.push(b[j + 1] as char);
                            j += 2;
                        } else {
                            key.push(b[j] as char);
                            j += 1;
                        }
                    }
                    if j >= b.len() {
                        return None;
                    }
                    segs.push(PathSeg::Key(key));
                    i = j + 1;
                } else {
                    let start = i;
                    while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                        i += 1;
                    }
                    if start == i {
                        return None;
                    }
                    segs.push(PathSeg::Key(
                        std::str::from_utf8(&b[start..i]).ok()?.to_string(),
                    ));
                }
            }
            b'[' => {
                let close = b[i..].iter().position(|&c| c == b']')? + i;
                let inner = std::str::from_utf8(&b[i + 1..close]).ok()?;
                if let Some(rest) = inner.strip_prefix("#-") {
                    let n: i64 = rest.parse().ok()?;
                    segs.push(PathSeg::Index(-n));
                } else if inner == "#" {
                    segs.push(PathSeg::Index(-0)); // # alone == 0 from end? SQLite: array length marker
                } else if let Ok(n) = inner.parse::<i64>() {
                    segs.push(PathSeg::Index(n));
                } else {
                    // ['key'] form
                    let k = inner.trim_matches(|c| c == '\'' || c == '"');
                    if k.is_empty() {
                        return None;
                    }
                    segs.push(PathSeg::Key(k.to_string()));
                }
                i = close + 1;
            }
            _ => return None,
        }
    }
    Some(JsonPath { segs })
}

impl JsonPath {
    /// Resolve against a document. Returns None when any step misses.
    pub fn resolve<'a>(&self, root: &'a Json) -> Option<&'a Json> {
        let mut cur = root;
        for seg in &self.segs {
            cur = match (seg, cur) {
                (PathSeg::Key(k), Json::Object(members)) => {
                    members.iter().find(|(mk, _)| mk == k).map(|(_, v)| v)?
                }
                (PathSeg::Index(i), Json::Array(items)) => {
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
}

// ---------------------------------------------------------------------------
// SQL value conversion
// ---------------------------------------------------------------------------

/// JSON node → SQL value (SQLite's json_extract semantics: strings are
/// unquoted text, numbers become Integer/Real, arrays/objects become their
/// JSON text).
pub fn json_to_sql(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::True => Value::Integer(1),
        Json::False => Value::Integer(0),
        Json::Integer(i) => Value::Integer(*i),
        Json::Real(r) => Value::Real(*r),
        Json::Str(s) | Json::Text(s) => Value::Text(s.clone().into()),
        Json::Array(_) | Json::Object(_) => Value::Text(json_to_string(j).into()),
    }
}

/// JSON-quote a SQL value for embedding in a JSON document text
/// (json_group_array / json_group_object accumulation).
pub fn json_quote_value(v: &Value) -> String {
    match v {
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => {
            if r.is_finite() {
                format!("{}", r)
            } else {
                "null".to_string()
            }
        }
        Value::Null => "null".to_string(),
        Value::Text(t) => {
            let s = t.to_string();
            let mut out = String::with_capacity(s.len() + 2);
            out.push('"');
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        out.push_str(&format!("\\u{:04x}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
            out
        }
        Value::Blob(_) => "null".to_string(),
    }
}

/// SQL value → JSON node (json_quote semantics).
pub fn sql_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Integer(*i),
        Value::Real(r) => Json::Real(*r),
        Value::Text(s) => Json::Str(s.as_str().to_owned()),
        Value::Blob(b) => Json::Str(String::from_utf8_lossy(b).to_string()),
    }
}

// ---------------------------------------------------------------------------
// Path rewriting for the mutation functions
// ---------------------------------------------------------------------------

/// Set the value at `path` inside `root` (creating intermediate objects for
/// json_set/json_insert with `create` = true). Returns the new document.
fn json_set_at(
    root: Json,
    path: &JsonPath,
    new_val: Json,
    create: bool,
    insert_only: bool,
) -> Json {
    fn walk(node: Json, segs: &[PathSeg], new_val: &Json, create: bool, insert_only: bool) -> Json {
        if segs.is_empty() {
            if insert_only {
                return node; // json_insert: only if absent — absent means we'd
                             // have created it; handled by caller returning early
            }
            return new_val.clone();
        }
        match (&segs[0], node) {
            (PathSeg::Key(k), Json::Object(mut members)) => {
                if let Some(pos) = members.iter().position(|(mk, _)| mk == k) {
                    let child = std::mem::replace(&mut members[pos].1, Json::Null);
                    members[pos].1 = walk(child, &segs[1..], new_val, create, insert_only);
                    Json::Object(members)
                } else if create {
                    let child = Json::Object(Vec::new());
                    members.push((
                        k.clone(),
                        walk(child, &segs[1..], new_val, create, insert_only),
                    ));
                    Json::Object(members)
                } else {
                    Json::Object(members)
                }
            }
            (PathSeg::Index(i), Json::Array(mut items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx >= 0 && (idx as usize) < items.len() {
                    let pos = idx as usize;
                    let child = std::mem::replace(&mut items[pos], Json::Null);
                    items[pos] = walk(child, &segs[1..], new_val, create, insert_only);
                } else if create && idx >= 0 && (idx as usize) == items.len() {
                    // append
                    let child = Json::Object(Vec::new());
                    items.push(walk(child, &segs[1..], new_val, create, insert_only));
                }
                Json::Array(items)
            }
            (_, node) => node,
        }
    }
    walk(root, &path.segs, &new_val, create, insert_only)
}

/// Remove the value at `path`. Returns the new document (unchanged when the
/// path doesn't resolve).
fn json_remove_at(root: Json, path: &JsonPath) -> Json {
    fn walk(node: Json, segs: &[PathSeg]) -> Json {
        if segs.is_empty() {
            return node;
        }
        if segs.len() == 1 {
            return match (&segs[0], node) {
                (PathSeg::Key(k), Json::Object(mut members)) => {
                    members.retain(|(mk, _)| mk != k);
                    Json::Object(members)
                }
                (PathSeg::Index(i), Json::Array(mut items)) => {
                    let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                    if idx >= 0 && (idx as usize) < items.len() {
                        items.remove(idx as usize);
                    }
                    Json::Array(items)
                }
                (_, node) => node,
            };
        }
        match (&segs[0], node) {
            (PathSeg::Key(k), Json::Object(mut members)) => {
                if let Some(pos) = members.iter().position(|(mk, _)| mk == k) {
                    let child = std::mem::replace(&mut members[pos].1, Json::Null);
                    members[pos].1 = walk(child, &segs[1..]);
                }
                Json::Object(members)
            }
            (PathSeg::Index(i), Json::Array(mut items)) => {
                let idx = if *i < 0 { items.len() as i64 + i } else { *i };
                if idx >= 0 && (idx as usize) < items.len() {
                    let pos = idx as usize;
                    let child = std::mem::replace(&mut items[pos], Json::Null);
                    items[pos] = walk(child, &segs[1..]);
                }
                Json::Array(items)
            }
            (_, node) => node,
        }
    }
    walk(root, &path.segs)
}

/// RFC 7396 JSON Merge Patch.
fn json_patch_target(target: &Json, patch: &Json) -> Json {
    match patch {
        Json::Object(patch_members) => {
            let mut out = match target {
                Json::Object(m) => m.clone(),
                _ => Vec::new(),
            };
            for (k, pv) in patch_members {
                if matches!(pv, Json::Null) {
                    out.retain(|(mk, _)| mk != k);
                } else {
                    let existing = out.iter().find(|(mk, _)| mk == k).map(|(_, v)| v.clone());
                    let merged = match existing {
                        Some(e) => json_patch_target(&e, pv),
                        None => json_patch_target(&Json::Object(Vec::new()), pv),
                    };
                    if let Some(pos) = out.iter().position(|(mk, _)| mk == k) {
                        out[pos].1 = merged;
                    } else {
                        out.push((k.clone(), merged));
                    }
                }
            }
            Json::Object(out)
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Public entry: the JSON1 scalar functions
// ---------------------------------------------------------------------------

/// Dispatch a JSON1 function call. `args` are the evaluated SQL arguments.
/// Returns None when `fname` isn't a JSON function (caller falls through).
pub fn call_json_function(fname: &str, args: &[Value]) -> Option<Value> {
    match fname {
        "json_valid" => Some(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let s = v.as_text();
                Value::Integer(if parse_json(&s).is_some() { 1 } else { 0 })
            }
        }),
        "json" => Some(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => match parse_json(&v.as_text()) {
                Some(j) => Value::Text(json_to_string(&j).into()),
                None => Value::Text(format!("malformed JSON: {}", trunc(&v.as_text())).into()),
            },
        }),
        "json_type" => {
            let (doc, path) = doc_and_path(args)?;
            Some(match path {
                Some(p) => match p.resolve(&doc) {
                    Some(node) => Value::Text(node.type_name().to_string().into()),
                    None => Value::Null,
                },
                None => Value::Text(doc.type_name().to_string().into()),
            })
        }
        "json_quote" => Some(match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => {
                let j = sql_to_json(v);
                Value::Text(json_to_string(&j).into())
            }
        }),
        "json_array" => {
            let items: Vec<Json> = args.iter().map(sql_to_json).collect();
            Some(Value::Text(json_to_string(&Json::Array(items)).into()))
        }
        "json_object" => {
            // json_object(k1, v1, k2, v2, ...) — odd arg count is an error
            // in SQLite; we mirror by returning a malformed marker text.
            if args.len() % 2 != 0 {
                return Some(Value::Text(
                    "json_object() requires an even number of arguments"
                        .to_string()
                        .into(),
                ));
            }
            let mut members = Vec::with_capacity(args.len() / 2);
            let mut i = 0;
            while i + 1 < args.len() {
                let k = args[i].as_text();
                members.push((k, sql_to_json(&args[i + 1])));
                i += 2;
            }
            Some(Value::Text(json_to_string(&Json::Object(members)).into()))
        }
        "json_array_length" => {
            let (doc, path) = doc_and_path(args)?;
            let target = match &path {
                Some(p) => p.resolve(&doc).cloned(),
                None => Some(doc.clone()),
            };
            Some(match target {
                Some(Json::Array(items)) => Value::Integer(items.len() as i64),
                Some(_) => Value::Integer(0),
                None => Value::Integer(0),
            })
        }
        "json_extract" => {
            // json_extract(x, path1, path2, ...): single path → the value;
            // multiple paths → a JSON array of the values. No matching path
            // → NULL (SQLite behavior for a single path; multiple paths
            // yield an array of nulls... we return NULL only when ALL miss).
            if args.len() < 2 {
                return Some(Value::Null);
            }
            let doc = parse_json(&args[0].as_text())?;
            let mut results = Vec::with_capacity(args.len() - 1);
            let mut any = false;
            for p in &args[1..] {
                let path = parse_path(&p.as_text());
                match path.and_then(|pp| pp.resolve(&doc).cloned()) {
                    Some(node) => {
                        any = true;
                        results.push(json_to_sql(&node));
                    }
                    None => results.push(Value::Null),
                }
            }
            if args.len() == 2 {
                Some(if any {
                    results.pop().unwrap()
                } else {
                    Value::Null
                })
            } else {
                Some(Value::Text(
                    json_to_string(&Json::Array(
                        results.into_iter().map(|v| sql_to_json(&v)).collect(),
                    ))
                    .into(),
                ))
            }
        }
        "json_insert" | "json_replace" | "json_set" => {
            // (doc, path, value [, path, value ...])
            if args.len() < 3 || args.len() % 2 == 0 {
                return Some(Value::Null);
            }
            let mut doc = parse_json(&args[0].as_text())?;
            let mut i = 1;
            while i + 1 < args.len() {
                let Some(path) = parse_path(&args[i].as_text()) else {
                    return Some(Value::Text("bad JSON path".to_string().into()));
                };
                let new_val = sql_to_json(&args[i + 1]);
                let exists = path.resolve(&doc).is_some();
                let apply = match fname {
                    // insert: only when absent
                    "json_insert" => !exists,
                    // replace: only when present
                    "json_replace" => exists,
                    // set: always
                    _ => true,
                };
                if apply {
                    doc = json_set_at(doc, &path, new_val, fname != "json_replace", false);
                }
                i += 2;
            }
            Some(Value::Text(json_to_string(&doc).into()))
        }
        "json_remove" => {
            // (doc, path...)
            if args.is_empty() {
                return Some(Value::Null);
            }
            let mut doc = parse_json(&args[0].as_text())?;
            for p in &args[1..] {
                if let Some(path) = parse_path(&p.as_text()) {
                    doc = json_remove_at(doc, &path);
                }
            }
            Some(Value::Text(json_to_string(&doc).into()))
        }
        "json_patch" => {
            if args.len() != 2 {
                return Some(Value::Null);
            }
            let target = parse_json(&args[0].as_text())?;
            let patch = parse_json(&args[1].as_text())?;
            Some(Value::Text(
                json_to_string(&json_patch_target(&target, &patch)).into(),
            ))
        }
        _ => None,
    }
}

fn doc_and_path(args: &[Value]) -> Option<(Json, Option<JsonPath>)> {
    let doc = parse_json(&args.first()?.as_text())?;
    let path = match args.get(1) {
        Some(p) => Some(parse_path(&p.as_text())?),
        None => None,
    };
    Some((doc, path))
}

fn trunc(s: &str) -> String {
    s.chars().take(32).collect()
}
