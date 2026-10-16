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
    self, jb_patch, jb_remove_at, jb_set_at, parse_lenient, parse_lenient_bytes, value_to_jb, Jb,
};
use crate::sql::ast::Expr;
use crate::types::Value;

// ---------------------------------------------------------------------------
// Path language
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum PathSeg {
    Key(String),
    Index(i64), // negative = from end
    /// `[#]`: one past the last element — never selects anything; the
    /// mutators append there.
    Append,
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
                    // `[#-0]` is `[#]`: the count itself, past the end.
                    segs.push(if n == 0 {
                        PathSeg::Append
                    } else {
                        PathSeg::Index(-n)
                    });
                } else if inner == "#" {
                    segs.push(PathSeg::Append);
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

/// Recently parsed JSON TEXT documents (SQLite's per-statement JsonCache):
/// one row's document is typically read by several expressions — `x->>'a'`
/// in each of three expression indexes, old and new keys on UPDATE, the
/// SET expression itself — and a 100 KB parse dwarfs everything else.
/// Keyed by CONTENT (length + a sampled fingerprint, then a full byte
/// compare), so a hit is always the same text; most recent last.
const DOC_CACHE_SLOTS: usize = 4;
/// Smaller documents parse faster than the cache can be probed.
const DOC_CACHE_MIN: usize = 256;

type DocCacheEntry = (u64, crate::types::Text, std::rc::Rc<Jb>);

thread_local! {
    static DOC_CACHE: std::cell::RefCell<Vec<DocCacheEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

type BlobCacheEntry = (u64, crate::types::Text, std::rc::Rc<Vec<u8>>);

thread_local! {
    /// TEXT documents parsed straight to JSONB (`parse_lenient_to_jsonb`)
    /// for the byte-level consumers — `->` / `->>` path lookups and the
    /// in-place edits of json_set & co. Same keying as `DOC_CACHE`.
    static BLOB_CACHE: std::cell::RefCell<Vec<BlobCacheEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The JSONB bytes of a JSON TEXT document, through the cache.
pub(crate) fn cached_text_jsonb(t: &crate::types::Text) -> Result<std::rc::Rc<Vec<u8>>, Error> {
    let bytes = t.as_bytes();
    let parse = || {
        jsonb::parse_lenient_to_jsonb(bytes)
            .map_err(|_| Error::Runtime("malformed JSON".to_string()))
    };
    if bytes.len() < DOC_CACHE_MIN {
        return parse().map(std::rc::Rc::new);
    }
    let fp = doc_fingerprint(bytes);
    let hit = BLOB_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let i = c
            .iter()
            .position(|(f, k, _)| *f == fp && k.as_bytes() == bytes)?;
        let e = c.remove(i);
        let blob = e.2.clone();
        c.push(e);
        Some(blob)
    });
    if let Some(blob) = hit {
        return Ok(blob);
    }
    let blob = std::rc::Rc::new(parse()?);
    blob_cache_put(fp, t.clone(), blob.clone());
    Ok(blob)
}

fn blob_cache_put(fp: u64, t: crate::types::Text, blob: std::rc::Rc<Vec<u8>>) {
    BLOB_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= DOC_CACHE_SLOTS {
            c.remove(0);
        }
        c.push((fp, t, blob));
    });
}

/// Path lookup on JSONB bytes: `Some(found)` (`None` inside = no such
/// path), or `None` when the bytes cannot be walked (callers fall back).
pub(crate) fn jsonb_lookup_bytes(b: &[u8], segs: &[PathSeg]) -> Option<Option<Jb>> {
    if !jsonb::jsonb_root_fits(b) {
        return None;
    }
    match jsonb::jsonb_locate(b, segs) {
        Ok(None) => Some(None),
        Ok(Some((s, e))) => jsonb::jsonb_decode_span(b, s, e).ok().map(Some),
        Err(()) => None,
    }
}

/// An edit pass over a JSONB buffer: `Ok(Some(true))` edited,
/// `Ok(Some(false))` the root was removed, `Ok(None)` declined.
type BlobEdits<'a> = dyn FnMut(&mut Vec<u8>) -> Result<Option<bool>, Error> + 'a;

/// json_set/insert/replace/remove over a JSON TEXT document, edited as
/// JSONB bytes in place (SQLite parses to JSONB and edits the blob too).
/// `Ok(None)`: an edit the byte walker declines — the caller takes the
/// tree path. A TEXT result enters the bytes cache with its blob.
fn text_doc_edit(
    t: &crate::types::Text,
    out: &Out,
    edits: &mut BlobEdits<'_>,
) -> Result<Option<Value>, Error> {
    let mut buf = (*cached_text_jsonb(t)?).clone();
    match edits(&mut buf)? {
        None => Ok(None),
        Some(false) => Ok(Some(Value::Null)), // the root was removed
        Some(true) => Ok(Some(match out {
            Out::Blob => Value::Blob(buf),
            Out::Text => {
                let Some(text) = jsonb::render_jsonb(&buf) else {
                    return Ok(None);
                };
                if text.as_bytes().len() >= DOC_CACHE_MIN {
                    blob_cache_put(
                        doc_fingerprint(text.as_bytes()),
                        text.clone(),
                        std::rc::Rc::new(buf),
                    );
                }
                Value::Text(text)
            }
        })),
    }
}

/// Length plus 64 sampled bytes — cheap next to the parse, and the full
/// compare on a match makes collisions harmless.
fn doc_fingerprint(b: &[u8]) -> u64 {
    let mut h: u64 = b.len() as u64;
    let step = (b.len() / 64).max(1);
    let mut i = 0;
    while i < b.len() {
        h = (h ^ b[i] as u64).wrapping_mul(0x100_0000_01b3);
        i += step;
    }
    h
}

/// The parsed tree of a JSON TEXT document, through the cache.
fn cached_text_doc(t: &crate::types::Text) -> Result<std::rc::Rc<Jb>, Error> {
    let bytes = t.as_bytes();
    let parse =
        || parse_lenient_bytes(bytes).map_err(|_| Error::Runtime("malformed JSON".to_string()));
    if bytes.len() < DOC_CACHE_MIN {
        return parse().map(std::rc::Rc::new);
    }
    let fp = doc_fingerprint(bytes);
    let hit = DOC_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let i = c
            .iter()
            .position(|(f, k, _)| *f == fp && k.as_bytes() == bytes)?;
        let e = c.remove(i);
        let doc = e.2.clone();
        c.push(e);
        Some(doc)
    });
    if let Some(doc) = hit {
        return Ok(doc);
    }
    let doc = std::rc::Rc::new(parse()?);
    DOC_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= DOC_CACHE_SLOTS {
            c.remove(0);
        }
        c.push((fp, t.clone(), doc.clone()));
    });
    Ok(doc)
}

/// `load_doc` for an editing caller: a TEXT document comes from the parse
/// cache (a copy of the cached tree, or the tree itself when the cache
/// does not hold it) — the UPDATE that edits a row has usually just read
/// it (`SET x = json_set(x, '$.e', (x->>'f') - 1)`).
fn load_doc_owned_cached(v: &Value) -> Result<Option<Jb>, Error> {
    match v {
        Value::Text(t) => {
            let rc = cached_text_doc(t)?;
            Ok(Some(
                std::rc::Rc::try_unwrap(rc).unwrap_or_else(|rc| (*rc).clone()),
            ))
        }
        other => load_doc(other),
    }
}

/// `out.emit(&doc)`, and a TEXT result enters the parse cache with the
/// tree already in hand: the edited row's expression-index keys and the
/// next statement's reads of it then skip the re-parse.
fn emit_cached(doc: Jb, out: Out) -> Value {
    let v = out.emit(&doc);
    if let Value::Text(t) = &v {
        let bytes = t.as_bytes();
        if bytes.len() >= DOC_CACHE_MIN {
            let fp = doc_fingerprint(bytes);
            let doc = std::rc::Rc::new(doc);
            DOC_CACHE.with(|c| {
                let mut c = c.borrow_mut();
                if c.len() >= DOC_CACHE_SLOTS {
                    c.remove(0);
                }
                c.push((fp, t.clone(), doc));
            });
        }
    }
    v
}

/// `load_doc` for read-only use: TEXT documents come from the parse
/// cache without copying the tree.
pub(crate) fn load_doc_shared(v: &Value) -> Result<Option<std::rc::Rc<Jb>>, Error> {
    match v {
        Value::Text(t) => cached_text_doc(t).map(Some),
        other => Ok(load_doc(other)?.map(std::rc::Rc::new)),
    }
}

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
                    // A non-JSONB blob parses as JSON TEXT bytes.
                    if let Ok(jb) = parse_lenient_bytes(b) {
                        return Ok(Some(jb));
                    }
                    Err(Error::Runtime("malformed JSON".to_string()))
                }
            }
        }
        // Numeric arguments become their JSON number form directly
        // (SQLite renders JSON numbers from SQL REALs at 15 significant
        // digits — different from the 17-digit REAL→TEXT conversion).
        Value::Integer(_) | Value::Real(_) => Ok(Some(value_to_jb(v))),
        // A TEXT argument parses over its RAW BYTES: SQLite's JSON
        // parser accepts any byte >= 0x20 inside string literals, so
        // `json(CAST(x'22ff22' AS TEXT))` is the JSON string holding
        // the raw 0xff byte (not malformed).
        Value::Text(t) if t.len() >= DOC_CACHE_MIN => {
            let doc = cached_text_doc(t)?;
            Ok(Some(
                std::rc::Rc::try_unwrap(doc).unwrap_or_else(|rc| (*rc).clone()),
            ))
        }
        Value::Text(t) => parse_lenient_bytes(t.as_bytes())
            .map(Some)
            .map_err(|_| Error::Runtime("malformed JSON".to_string())),
    }
}

/// The element `segs` selects in a JSONB BLOB document, decoding only
/// that element: `Some(None)` = nothing selected, `Some(Some(jb))` = the
/// element. `None` = not applicable (not a JSONB blob, or a malformed
/// header on the walk) — callers fall back to `load_doc` + `jb_resolve`,
/// which validate and raise exactly as before.
pub(crate) fn jsonb_lookup(doc: &Value, segs: &[PathSeg]) -> Option<Option<Jb>> {
    let Value::Blob(b) = doc else {
        return None;
    };
    if !jsonb::jsonb_root_fits(b) {
        return None;
    }
    match jsonb::jsonb_locate(b, segs) {
        Ok(None) => Some(None),
        Ok(Some((s, e))) => jsonb::jsonb_decode_span(b, s, e).ok().map(Some),
        Err(()) => None,
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
            Out::Text => Value::Text(jb.render()),
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
            Some(v) => match load_doc_shared(v)? {
                None => Value::Null,
                Some(jb) => Value::Text(jb.render_pretty()),
            },
        },
        "json_error_position" => match args.first() {
            Some(Value::Null) | None => Value::Null,
            Some(v) => Value::Integer(match v {
                Value::Text(t) => match parse_lenient_bytes(t.as_bytes()) {
                    Ok(_) => 0,
                    Err(pos) => pos as i64,
                },
                Value::Blob(b) => {
                    let ok = parse_lenient_bytes(b).is_ok() || jsonb::jb_from_bytes(b).is_ok();
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
            if let (Some(d), Some(p)) = (args.first(), args.get(1)) {
                if !p.is_null() {
                    let path = parse_doc_path(p)?;
                    if let Some(found) = jsonb_lookup(d, path.segs()) {
                        return Ok(Some(match found {
                            Some(node) => Value::Text(node.type_name().into()),
                            None => Value::Null,
                        }));
                    }
                }
            }
            let Some(doc) = load_doc_shared(args.first().unwrap_or(&Value::Null))? else {
                return Ok(Some(Value::Null));
            };
            let target = match args.get(1) {
                Some(p) => {
                    let path = parse_doc_path(p)?;
                    jsonb::jb_resolve(&doc, path.segs())
                }
                None => Some(&*doc),
            };
            match target {
                Some(node) => Value::Text(node.type_name().into()),
                None => Value::Null,
            }
        }
        "json_array_length" => {
            let Some(doc) = load_doc_shared(args.first().unwrap_or(&Value::Null))? else {
                return Ok(Some(Value::Null));
            };
            let target = match args.get(1) {
                Some(p) => {
                    let path = parse_doc_path(p)?;
                    jsonb::jb_resolve(&doc, path.segs())
                }
                None => Some(&*doc),
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
            // jsonb_set -> json_set: the mode, minus the `b`.
            json_mutate(
                &format!("json_{}", &fname["jsonb_".len()..]),
                args,
                Out::Blob,
            )?
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
            if matches!(out, Out::Blob) && jsonb::jsonb_check(b).is_some() {
                // Validation only — no tree for the verbatim copy.
                return Ok(Value::Blob(b.clone()));
            }
            match jsonb::jb_from_bytes(b) {
                Ok(jb) => match out {
                    Out::Blob => Ok(Value::Blob(b.clone())),
                    Out::Text => Ok(Value::Text(jb.render())),
                },
                Err(()) => {
                    // A non-JSONB blob parses as JSON TEXT bytes.
                    if let Ok(jb) = parse_lenient_bytes(b) {
                        return Ok(out.emit(&jb));
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
            // TEXT parses over its RAW bytes (SQLite's JSON parser
            // accepts any byte >= 0x20 inside string literals).
            let bytes: Vec<u8> = match other {
                Value::Text(t) => t.as_bytes().to_vec(),
                o => o.as_text().into_bytes(),
            };
            match parse_lenient_bytes(&bytes) {
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
                Ok(jb) => Ok(Value::Text(jb.render())),
                Err(()) => Ok(Value::Text("null".into())),
            }
        }
        Some(v) => {
            let jb = value_to_jb(v);
            Ok(Value::Text(jb.render()))
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
    if matches!(out, Out::Blob) {
        return jsonb_object_spliced(args);
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

/// `jsonb_object(...)`: encode straight into the output payload. A
/// canonical JSONB blob argument is copied verbatim — byte-identical to
/// decoding and re-encoding it, without the tree (speedtest1 3:100 nests
/// two ~45 KB JSONB values into each of 500 rows). A non-canonical one
/// still re-encodes (SQLite shortens every header).
fn jsonb_object_spliced(args: &[Value]) -> Result<Value, Error> {
    let mut payload: Vec<u8> = Vec::new();
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
        jsonb::encode_elem_pub(&mut payload, &key);
        match &args[i + 1] {
            Value::Blob(b) if jsonb::jsonb_check(b) == Some(true) => {
                payload.extend_from_slice(b);
            }
            v => jsonb::encode_elem_pub(&mut payload, &value_to_jb(v)),
        }
        i += 2;
    }
    let mut out = Vec::with_capacity(payload.len() + 9);
    jsonb::push_container(&mut out, jsonb::T_OBJECT, &payload);
    Ok(Value::Blob(out))
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
    if args.len() == 2 && !args[1].is_null() {
        let path = parse_doc_path(&args[1])?;
        if let Some(found) = jsonb_lookup(&args[0], path.segs()) {
            return Ok(match found {
                Some(node) => match out {
                    Out::Text => node.to_value(),
                    Out::Blob => match node {
                        Jb::Array(_) | Jb::Object(_) => Value::Blob(jsonb::jb_to_bytes(&node)),
                        scalar => scalar.to_value(),
                    },
                },
                None => Value::Null,
            });
        }
    }
    let Some(doc) = load_doc_shared(&args[0])? else {
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
    // A JSONB blob producing a JSONB blob: edit the bytes in place
    // (SQLite's jsonBlobEdit) instead of decoding the whole document.
    if let (Out::Blob, Value::Blob(b)) = (&out, &args[0]) {
        if jsonb::jsonb_root_fits(b) {
            let mut buf = b.to_vec();
            let mut ok = true;
            let mut i = 1;
            while i + 1 < args.len() {
                let path = parse_doc_path(&args[i])?;
                let tree = sql_value_as_raw(value_to_jb(&args[i + 1]));
                let val = jsonb::jb_to_bytes(&tree);
                let edit = jsonb::JbEdit::Put {
                    val: &val,
                    tree: &tree,
                    create: fname != "json_replace",
                    overwrite: fname != "json_insert",
                };
                if jsonb::jsonb_edit(&mut buf, path.segs(), &edit).is_err() {
                    ok = false;
                    break;
                }
                i += 2;
            }
            if ok {
                return Ok(Value::Blob(buf));
            }
        }
    }
    if let Value::Text(t) = &args[0] {
        let r = text_doc_edit(t, &out, &mut |buf| {
            let mut i = 1;
            while i + 1 < args.len() {
                let path = parse_doc_path(&args[i])?;
                let tree = sql_value_as_raw(value_to_jb(&args[i + 1]));
                let val = jsonb::jb_to_bytes(&tree);
                let edit = jsonb::JbEdit::Put {
                    val: &val,
                    tree: &tree,
                    create: fname != "json_replace",
                    overwrite: fname != "json_insert",
                };
                if jsonb::jsonb_edit(buf, path.segs(), &edit).is_err() {
                    return Ok(None);
                }
                i += 2;
            }
            Ok(Some(true))
        })?;
        if let Some(v) = r {
            return Ok(v);
        }
    }
    let Some(mut doc) = load_doc_owned_cached(&args[0])? else {
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
    Ok(emit_cached(doc, out))
}

/// SQLite's mutation functions (json_set/insert/replace) insert SQL TEXT
/// values as TEXTRAW elements — the payload stays raw and escapes are
/// added lazily at render time. Numbers, nulls, and JSONB-blob embeds
/// pass through untouched.
fn sql_value_as_raw(jb: Jb) -> Jb {
    match jb {
        Jb::Text(s) => Jb::TextRaw(s),
        Jb::TextJ(s) => Jb::TextRaw(crate::types::Text::from_bytes(&jsonb::decode_escapes(
            s.as_bytes(),
            false,
        ))),
        other => other,
    }
}

/// `json_remove(X, P...)` — removing the root path yields SQL NULL.
fn json_remove(args: &[Value], out: Out) -> Result<Value, Error> {
    if args.is_empty() {
        return Ok(Value::Null);
    }
    if let (Out::Blob, Value::Blob(b)) = (&out, &args[0]) {
        if jsonb::jsonb_root_fits(b) {
            let mut buf = b.to_vec();
            let mut ok = true;
            for p in &args[1..] {
                let path = parse_doc_path(p)?;
                match jsonb::jsonb_edit(&mut buf, path.segs(), &jsonb::JbEdit::Remove) {
                    Ok(true) => {}
                    Ok(false) => return Ok(Value::Null),
                    Err(()) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                return Ok(Value::Blob(buf));
            }
        }
    }
    if let Value::Text(t) = &args[0] {
        let r = text_doc_edit(t, &out, &mut |buf| {
            for p in &args[1..] {
                let path = parse_doc_path(p)?;
                match jsonb::jsonb_edit(buf, path.segs(), &jsonb::JbEdit::Remove) {
                    Ok(true) => {}
                    Ok(false) => return Ok(Some(false)),
                    Err(()) => return Ok(None),
                }
            }
            Ok(Some(true))
        })?;
        if let Some(v) = r {
            return Ok(v);
        }
    }
    let Some(mut doc) = load_doc_owned_cached(&args[0])? else {
        return Ok(Value::Null);
    };
    for p in &args[1..] {
        let path = parse_doc_path(p)?;
        match jb_remove_at(doc, path.segs()) {
            Some(new_doc) => doc = new_doc,
            None => return Ok(Value::Null), // path '$' removes the document
        }
    }
    Ok(emit_cached(doc, out))
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
        valid |= jsonb::parse_strict_bytes(bytes).is_ok();
    }
    if flags & 2 != 0 {
        valid |= jsonb::parse_lenient_bytes(bytes).is_ok();
    }
    if flags & 12 != 0 {
        valid |= jsonb::jb_from_bytes(bytes).is_ok();
    }
    Ok(Value::Integer(if valid { 1 } else { 0 }))
}

// ---------------------------------------------------------------------------
// JSON subtype (SQLite's 'J' value subtype)
// ---------------------------------------------------------------------------

/// Does `e` produce a value that carries SQLite's JSON subtype — text a
/// JSON constructor must EMBED as JSON rather than quote as a string?
/// The JSON-returning functions and the `->` operator always do
/// (`json_array(json('[1]'))` is `[[1]]`, not `["[1]"]`); json_extract()
/// does only for an array / object result (`json_subtype_value` checks).
pub(crate) fn expr_has_json_subtype(e: &Expr) -> bool {
    match e {
        Expr::Function {
            name, over: None, ..
        } => matches!(
            name.to_ascii_lowercase().as_str(),
            "json"
                | "json_array"
                | "json_object"
                | "json_set"
                | "json_insert"
                | "json_replace"
                | "json_remove"
                | "json_patch"
                | "json_quote"
                | "json_group_array"
                | "json_group_object"
                | "json_extract"
        ),
        Expr::Binary {
            op: crate::sql::ast::BinaryOp::Arrow,
            ..
        } => true,
        _ => false,
    }
}

/// The argument positions of a JSON constructor that take a VALUE (and so
/// honor the JSON subtype); `None` for functions that ignore subtypes.
fn json_value_position(fname: &str, i: usize) -> bool {
    match fname {
        "json_array" | "jsonb_array" => true,
        "json_object" | "jsonb_object" | "__json_object_frag" | "__jsonb_object_frag" => i % 2 == 1,
        "json_set" | "json_insert" | "json_replace" | "jsonb_set" | "jsonb_insert"
        | "jsonb_replace" => i >= 2 && i % 2 == 0,
        _ => false,
    }
}

/// Apply the JSON subtype to a constructor's evaluated arguments: a TEXT
/// value produced by a JSON function (see `expr_has_json_subtype`) is
/// handed over as its JSONB encoding, which every constructor already
/// embeds as JSON. json_extract() qualifies only when its result is an
/// array or object (its scalar results are plain SQL values).
pub(crate) fn apply_json_subtypes(fname: &str, args: &[Expr], vals: &mut [Value]) {
    for (i, (a, v)) in args.iter().zip(vals.iter_mut()).enumerate() {
        if !json_value_position(fname, i) || !expr_has_json_subtype(a) || is_json_extract_call(a) {
            continue; // json_extract: decided from its node (json_extract_subtyped)
        }
        let Value::Text(t) = &*v else { continue };
        let Ok(jb) = parse_lenient(t) else { continue };
        *v = Value::Blob(jsonb::jb_to_bytes(&jb));
    }
}

/// Is `e` a json_extract(...) call (whose JSON subtype depends on the
/// extracted node — see `json_extract_subtyped`)?
pub(crate) fn is_json_extract_call(e: &Expr) -> bool {
    matches!(e, Expr::Function { name, over: None, .. } if name.eq_ignore_ascii_case("json_extract"))
}

/// May argument `i` of `fname` consume a JSON subtype?
pub(crate) fn takes_json_value(fname: &str, i: usize) -> bool {
    json_value_position(fname, i)
}

/// json_extract(X, P...) as a JSON-subtype-aware VALUE: the JSONB
/// encoding when SQLite's result carries the JSON subtype — several
/// paths (always an array), or one path selecting an array / object —
/// else exactly json_extract's SQL value (a string "[1]" stays text).
pub(crate) fn json_extract_subtyped(args: &[Value]) -> Result<Value, Error> {
    let v = json_extract(args, Out::Text)?;
    let subtyped = match args.len() {
        0 | 1 => false,
        2 => {
            let Some(doc) = load_doc_shared(&args[0])? else {
                return Ok(v);
            };
            let path = parse_doc_path(&args[1])?;
            matches!(
                jsonb::jb_resolve(&doc, path.segs()),
                Some(Jb::Array(_) | Jb::Object(_))
            )
        }
        _ => true,
    };
    if subtyped {
        if let Value::Text(t) = &v {
            if let Ok(jb) = parse_lenient(t) {
                return Ok(Value::Blob(jsonb::jb_to_bytes(&jb)));
            }
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Legacy helpers kept for the aggregate accumulators
// ---------------------------------------------------------------------------

/// JSON-quote a SQL value for embedding in a JSON document text
/// (json_group_array / json_group_object accumulation). Returns the
/// element's RAW rendered bytes — a raw (invalid-UTF-8) text value
/// quotes with its bytes as-is, exactly as SQLite's jsonAppendString.
pub fn json_quote_value(v: &Value) -> crate::types::Text {
    match v {
        Value::Null => crate::types::Text::from("null"),
        Value::Integer(i) => crate::types::Text::from(i.to_string()),
        Value::Real(r) => crate::types::Text::from(jsonb::render_sql_f64(*r)),
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
