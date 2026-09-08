//! JSON1 support: the SQLite JSON SQL functions on the raw-preserving
//! JSONB pipeline (`crate::executor::jsonb`).
//!
//! Everything SQLite does round-trips: `json('[1e2, 1.50]')` →
//! `'[1e2,1.50]'` (original numeric text kept), `json('"a\u0062"')` →
//! verbatim escapes, JSONB blobs are accepted by every function, and
//! malformed input RAISES `malformed JSON` like SQLite instead of
//! returning an error text.
//!
//! Path syntax: `$`, `.key`, `."quoted key"`, `[index]`, `[#-n]`, and
//! chained forms — SQLite's path language subset used by the engine.

use crate::error::Error;
use crate::executor::jsonb::{
    self, jb_patch, jb_remove_at, jb_set_at, parse_lenient, parse_strict, value_to_jb, Jb,
};
use crate::types::Value;

// ---------------------------------------------------------------------------
// Path language
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum PathSeg {
    Key(String),
    Index(i64), // negative = from end
}

#[derive(Clone, Debug)]
pub struct JsonPath {
    segs: Vec<PathSeg>,
}

impl JsonPath {
    pub fn segs(&self) -> &[PathSeg] {
        &self.segs
    }
}

/// Parse a JSON path like `$`, `$.a.b`, `$[0].x`, `."q k"`, `[#-1]`.
/// Returns None on malformed paths.
pub fn parse_path(p: &str) -> Option<JsonPath> {
    let b = p.as_bytes();
    let mut segs = Vec::new();
    if b.first() != Some(&b'$') {
        return None;
    }
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
                    segs.push(PathSeg::Index(-1)); // # alone = last element
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

// ---------------------------------------------------------------------------
// Document loading (text or JSONB blob, SQLite's dual-parse rules)
// ---------------------------------------------------------------------------

/// Parse the first argument of a JSON function: TEXT parses as JSON text,
/// BLOB parses as JSON text first (dual-valid blobs read as text), then
/// falls back to JSONB. NULL stays NULL; other scalars parse their text
/// form (`json_extract(5, '$')` works).
pub(crate) fn load_doc(v: &Value) -> Result<Option<Jb>, Error> {
    match v {
        Value::Null => Ok(None),
        Value::Blob(b) => {
            // JSONB-first (sqlite.org/jsonb.html §3.4: a well-formed
            // element that exactly fills the blob IS JSONB); text is the
            // fallback for blobs that fail the structural check.
            match jsonb::jb_from_bytes(b) {
                Ok(jb) => Ok(Some(jb)),
                Err(()) => {
                    if let Ok(s) = std::str::from_utf8(b) {
                        if let Ok(jb) = parse_lenient(s) {
                            return Ok(Some(jb));
                        }
                    }
                    Err(Error::Runtime("malformed JSON".to_string()))
                }
            }
        }
        // Numeric arguments become their JSON number form directly
        // (SQLite renders JSON numbers from SQL REALs at 15 significant
        // digits — different from the 17-digit REAL→TEXT conversion).
        Value::Integer(_) | Value::Real(_) => Ok(Some(value_to_jb(v))),
        other => parse_lenient(&other.as_text())
            .map(Some)
            .map_err(|_| Error::Runtime("malformed JSON".to_string())),
    }
}

fn bad_path(p: &str) -> Error {
    Error::Runtime(format!("bad JSON path: '{}'", p))
}

fn parse_doc_path(p: &Value) -> Result<JsonPath, Error> {
    let s = p.as_text();
    parse_path(&s).ok_or_else(|| bad_path(&s))
}

/// Render the output of a JSON-producing function: TEXT for the `json_*`
/// family, BLOB for the `jsonb_*` family.
enum Out {
    Text,
    Blob,
}

impl Out {
    fn emit(self, jb: &Jb) -> Value {
        match self {
            Out::Text => Value::Text(jb.render().into()),
            Out::Blob => Value::Blob(jsonb::jb_to_bytes(jb)),
        }
    }
}

// ---------------------------------------------------------------------------
// The JSON1 scalar functions
// ---------------------------------------------------------------------------

/// Dispatch a JSON1 function call. `args` are the evaluated SQL arguments.
/// Returns Ok(None) when `fname` isn't a JSON function (caller falls
/// through); errors propagate (SQLite raises on malformed JSON/paths).
pub fn call_json_function(fname: &str, args: &[Value]) -> Result<Option<Value>, Error> {
    let v = match fname {
        "json" => json_fn(args, Out::Text)?,
        "jsonb" => json_fn(args, Out::Blob)?,
        "json_quote" => json_quote(args)?,
        "json_pretty" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => match load_doc(v)? {
                None => Value::Null,
                Some(jb) => Value::Text(jb.render_pretty().into()),
            },
        },
        "json_error_position" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Integer(match v {
                Value::Text(t) => match parse_lenient(t) {
                    Ok(_) => 0,
                    Err(pos) => pos as i64,
                },
                Value::Blob(b) => {
                    let ok = (std::str::from_utf8(b)
                        .ok()
                        .map(|s| parse_lenient(s).is_ok()))
                    .unwrap_or(false)
                        || jsonb::jb_from_bytes(b).is_ok();
                    if ok {
                        0
                    } else {
                        1
                    }
                }
                _ => 0,
            }),
        },
        "json_valid" => json_valid(args)?,
        "json_type" => {
            let Some(doc) = load_doc(args.first().unwrap_or(&Value::Null))? else {
                return Ok(Some(Value::Null));
            };
            let target = match args.get(1) {
                Some(p) => {
                    let path = parse_doc_path(p)?;
                    jsonb::jb_resolve(&doc, path.segs())
                }
                None => Some(&doc),
            };
            match target {
                Some(node) => Value::Text(node.type_name().into()),
                None => Value::Null,
            }
        }
        "json_array_length" => {
            let Some(doc) = load_doc(args.first().unwrap_or(&Value::Null))? else {
                return Ok(Some(Value::Null));
            };
            let target = match args.get(1) {
                Some(p) => {
                    let path = parse_doc_path(p)?;
                    jsonb::jb_resolve(&doc, path.segs())
                }
                None => Some(&doc),
            };
            Value::Integer(match target {
                Some(Jb::Array(items)) => items.len() as i64,
                _ => 0,
            })
        }
        "json_array" => {
            let items: Vec<Jb> = args.iter().map(value_to_jb).collect();
            Out::Text.emit(&Jb::Array(items))
        }
        "jsonb_array" => {
            let items: Vec<Jb> = args.iter().map(value_to_jb).collect();
            Out::Blob.emit(&Jb::Array(items))
        }
        "json_object" => json_object(args, Out::Text)?,
        "jsonb_object" => json_object(args, Out::Blob)?,
        "json_extract" => json_extract(args, Out::Text)?,
        "jsonb_extract" => json_extract(args, Out::Blob)?,
        "json_set" | "json_insert" | "json_replace" => json_mutate(fname, args, Out::Text)?,
        "jsonb_set" | "jsonb_insert" | "jsonb_replace" => {
            json_mutate(&fname[..fname.len() - 1], args, Out::Blob)?
        }
        "json_remove" => json_remove(args, Out::Text)?,
        "jsonb_remove" => json_remove(args, Out::Blob)?,
        "json_patch" => {
            if args.len() != 2 {
                return Ok(Some(Value::Null));
            }
            let target = load_doc(&args[0])?;
            let patch = load_doc(&args[1])?;
            match (target, patch) {
                (Some(t), Some(p)) => Out::Text.emit(&jb_patch(&t, &p)),
                _ => Value::Null,
            }
        }
        "jsonb_patch" => {
            if args.len() != 2 {
                return Ok(Some(Value::Null));
            }
            let target = load_doc(&args[0])?;
            let patch = load_doc(&args[1])?;
            match (target, patch) {
                (Some(t), Some(p)) => Out::Blob.emit(&jb_patch(&t, &p)),
                _ => Value::Null,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// `json(X)` / `jsonb(X)`: parse, re-render. JSONB blobs that are valid
/// JSONB (and not valid text) round-trip VERBATIM.
fn json_fn(args: &[Value], out: Out) -> Result<Value, Error> {
    let Some(first) = args.first() else {
        return Ok(Value::Null);
    };
    match first {
        Value::Null => Ok(Value::Null),
        Value::Blob(b) => {
            // JSONB-first; a valid JSONB blob round-trips VERBATIM
            // through jsonb() (SQLite's identity, even for non-shortest
            // headers), and renders as text through json().
            match jsonb::jb_from_bytes(b) {
                Ok(jb) => match out {
                    Out::Blob => Ok(Value::Blob(b.clone())),
                    Out::Text => Ok(Value::Text(jb.render().into())),
                },
                Err(()) => {
                    if let Ok(s) = std::str::from_utf8(b) {
                        if let Ok(jb) = parse_lenient(s) {
                            return Ok(out.emit(&jb));
                        }
                    }
                    Err(Error::Runtime("malformed JSON".to_string()))
                }
            }
        }
        other => {
            if matches!(other, Value::Integer(_) | Value::Real(_)) {
                let jb = value_to_jb(other);
                return Ok(out.emit(&jb));
            }
            match parse_lenient(&other.as_text()) {
                Ok(jb) => Ok(out.emit(&jb)),
                Err(_) => Err(Error::Runtime("malformed JSON".to_string())),
            }
        }
    }
}

/// `json_quote(X)`: SQL value → JSON text (NULL quotes to the TEXT
/// 'null', matching SQLite).
fn json_quote(args: &[Value]) -> Result<Value, Error> {
    match args.first() {
        None | Some(Value::Null) => Ok(Value::Text("null".into())),
        Some(Value::Blob(b)) => {
            // A JSONB blob renders as its JSON text; anything else is null.
            match jsonb::blob_to_jb(b) {
                Ok(jb) => Ok(Value::Text(jb.render().into())),
                Err(()) => Ok(Value::Text("null".into())),
            }
        }
        Some(v) => {
            let jb = value_to_jb(v);
            Ok(Value::Text(jb.render().into()))
        }
    }
}

/// `json_object(k1, v1, ...)` / `jsonb_object(...)`.
fn json_object(args: &[Value], out: Out) -> Result<Value, Error> {
    if args.is_empty() {
        return Ok(out.emit(&Jb::Object(Vec::new())));
    }
    if args.len() % 2 != 0 {
        // SQLite raises; message matches sqlite3_errmsg.
        return Err(Error::Runtime(
            "json_object() requires an even number of arguments".to_string(),
        ));
    }
    let mut pairs = Vec::with_capacity(args.len() / 2);
    let mut i = 0;
    while i + 1 < args.len() {
        let key = match &args[i] {
            Value::Null => {
                return Err(Error::Runtime(
                    "json_object() labels must be TEXT".to_string(),
                ))
            }
            v => value_to_jb(v),
        };
        if !key.is_string() {
            return Err(Error::Runtime(
                "json_object() labels must be TEXT".to_string(),
            ));
        }
        pairs.push((key, value_to_jb(&args[i + 1])));
        i += 2;
    }
    Ok(out.emit(&Jb::Object(pairs)))
}

/// `json_extract(X, P...)` / `jsonb_extract(...)`: single path → the value
/// (SQL semantics for json_, element for jsonb_); multiple paths → an
/// array of the results.
fn json_extract(args: &[Value], out: Out) -> Result<Value, Error> {
    if args.len() < 2 {
        return Err(Error::Runtime(
            "json_extract() requires at least two arguments".to_string(),
        ));
    }
    let Some(doc) = load_doc(&args[0])? else {
        return Ok(Value::Null);
    };
    let mut paths = Vec::with_capacity(args.len() - 1);
    for p in &args[1..] {
        paths.push(parse_doc_path(p)?);
    }
    if paths.len() == 1 {
        return Ok(match jsonb::jb_resolve(&doc, paths[0].segs()) {
            Some(node) => match out {
                // json_extract: SQL value semantics.
                Out::Text => node.to_value(),
                // jsonb_extract: the JSONB blob for CONTAINER targets;
                // scalars come back as their SQL values (SQLite returns
                // typeof integer/real/text/null for scalar paths).
                Out::Blob => match node {
                    Jb::Array(_) | Jb::Object(_) => Value::Blob(jsonb::jb_to_bytes(node)),
                    scalar => scalar.to_value(),
                },
            },
            None => Value::Null,
        });
    }
    // Multiple paths: array of per-path results. json_extract builds from
    // the extracted SQL VALUES (canonical round-trip); jsonb_extract keeps
    // the raw elements.
    let items: Vec<Jb> = paths
        .iter()
        .map(|p| match jsonb::jb_resolve(&doc, p.segs()) {
            Some(node) => match out {
                Out::Text => value_to_jb(&node.to_value()),
                Out::Blob => node.clone(),
            },
            None => Jb::Null,
        })
        .collect();
    Ok(out.emit(&Jb::Array(items)))
}

/// `json_set` / `json_insert` / `json_replace` (and the jsonb_ variants —
/// `fname` arrives WITHOUT the jsonb_ prefix here).
fn json_mutate(fname: &str, args: &[Value], out: Out) -> Result<Value, Error> {
    if args.len() < 3 || args.len() % 2 == 0 {
        return Err(Error::Runtime(format!(
            "{}() requires an odd number of arguments",
            fname
        )));
    }
    let Some(mut doc) = load_doc(&args[0])? else {
        return Ok(Value::Null);
    };
    let mut i = 1;
    while i + 1 < args.len() {
        let path = parse_doc_path(&args[i])?;
        let new_val = sql_value_as_raw(value_to_jb(&args[i + 1]));
        let exists = jsonb::jb_resolve(&doc, path.segs()).is_some();
        let apply = match fname {
            "json_insert" => !exists,
            "json_replace" => exists,
            _ => true, // json_set
        };
        if apply {
            doc = jb_set_at(doc, path.segs(), new_val, fname != "json_replace", false);
        }
        i += 2;
    }
    Ok(out.emit(&doc))
}

/// SQLite's mutation functions (json_set/insert/replace) insert SQL TEXT
/// values as TEXTRAW elements — the payload stays raw and escapes are
/// added lazily at render time. Numbers, nulls, and JSONB-blob embeds
/// pass through untouched.
fn sql_value_as_raw(jb: Jb) -> Jb {
    match jb {
        Jb::Text(s) => Jb::TextRaw(s),
        Jb::TextJ(s) => Jb::TextRaw(jsonb::decode_escapes(&s, false)),
        other => other,
    }
}

/// `json_remove(X, P...)` — removing the root path yields SQL NULL.
fn json_remove(args: &[Value], out: Out) -> Result<Value, Error> {
    if args.is_empty() {
        return Ok(Value::Null);
    }
    let Some(mut doc) = load_doc(&args[0])? else {
        return Ok(Value::Null);
    };
    for p in &args[1..] {
        let path = parse_doc_path(p)?;
        match jb_remove_at(doc, path.segs()) {
            Some(new_doc) => doc = new_doc,
            None => return Ok(Value::Null), // path '$' removes the document
        }
    }
    Ok(out.emit(&doc))
}

/// `json_valid(X)` (strict JSON) / `json_valid(X, flags)` — bit 1 = strict
/// RFC 8259 text, bit 2 = JSON5 text, bits 4|8 = JSONB blob; any enabled
/// check passing returns 1.
fn json_valid(args: &[Value]) -> Result<Value, Error> {
    let flags: i64 = match args.len() {
        1 => 1,
        2 => match &args[1] {
            Value::Null => return Ok(Value::Null),
            v => v.as_integer(),
        },
        _ => {
            return Err(Error::Runtime(
                "json_valid() takes at most two arguments".to_string(),
            ))
        }
    };
    if !(1..=15).contains(&flags) {
        return Err(Error::Runtime(
            "FLAGS parameter to json_valid() must be between 1 and 15".to_string(),
        ));
    }
    let Some(x) = args.first() else {
        return Ok(Value::Null);
    };
    let owned_bytes: Option<Vec<u8>> = match x {
        Value::Null => return Ok(Value::Null),
        Value::Text(t) => Some(t.as_bytes().to_vec()),
        Value::Blob(b) => Some(b.clone()),
        other => Some(other.as_text().into_bytes()),
    };
    let Some(bytes) = owned_bytes else {
        return Ok(Value::Integer(0));
    };
    let bytes: &[u8] = &bytes;
    let mut valid = false;
    if flags & 1 != 0 {
        if let Ok(s) = std::str::from_utf8(bytes) {
            valid |= parse_strict(s).is_ok();
        }
    }
    if flags & 2 != 0 {
        if let Ok(s) = std::str::from_utf8(bytes) {
            valid |= jsonb::parse_json5(s).is_ok();
        }
    }
    if flags & 12 != 0 {
        valid |= jsonb::jb_from_bytes(bytes).is_ok();
    }
    Ok(Value::Integer(if valid { 1 } else { 0 }))
}

// ---------------------------------------------------------------------------
// Legacy helpers kept for the aggregate accumulators
// ---------------------------------------------------------------------------

/// JSON-quote a SQL value for embedding in a JSON document text
/// (json_group_array / json_group_object accumulation).
pub fn json_quote_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(r) => jsonb::render_sql_f64(*r),
        Value::Text(_) | Value::Blob(_) => value_to_jb(v).render(),
    }
}

/// SQL value → JSON node (legacy decoded view for the old tree consumers).
pub fn sql_to_json(v: &Value) -> Jb {
    value_to_jb(v)
}

/// Parse a complete JSON document (decoded view) — legacy entry point.
pub fn parse_json(s: &str) -> Option<Jb> {
    parse_lenient(s).ok()
}
