//! Minimal, dependency-free JSON for the HTTP wire protocol.
//!
//! Scope: the exact dialect the server and CLI speak to each other —
//! objects, arrays, strings, numbers, `null`. It is a wire format for OUR
//! tools, not a general-purpose JSON stack; two small extensions beyond
//! RFC 8259 keep engine values round-trippable:
//!
//! * BLOBs ride as a tagged object `{"blob":"<hex>"}` (JSON has no binary
//!   type). `Json::to_value` / `Json::from_value` convert between this
//!   tagged form and [`crate::types::Value`].
//! * Non-finite REALs (`NaN`, `±inf`) serialize as `null` — JSON numbers
//!   cannot represent them (the old hand-rolled emitter printed bare
//!   `NaN`, which no JSON parser accepts).
//!
//! The parser also accepts Rust's `{:?}`-style string escapes
//! (`\u{41}` besides `\u0041`) because the server historically emitted
//! them; the serializer always emits standard JSON.

use crate::types::Value;

/// A parsed JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Array(Vec<Json>),
    /// Insertion-ordered key/value pairs (order matters for stable
    /// serialization).
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Parse a complete JSON document (trailing non-whitespace is an
    /// error). Depth-limited to 64 to keep adversarial input off the
    /// stack.
    pub fn parse(input: &str) -> Result<Json, String> {
        let mut p = Parser {
            b: input.as_bytes(),
            pos: 0,
            depth: 0,
        };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.pos != p.b.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(v)
    }

    /// First value stored under `key` (object shape).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::Real(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
            _ => Option::None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Convert a wire value to an engine [`Value`]. `Json::Str` becomes
    /// `Value::Text`; the tagged `{"blob": "<hex>"}` object becomes
    /// `Value::Blob`; `Json::Bool` becomes `0`/`1` (SQLite booleans).
    /// Other shapes (bare arrays/objects) are rejected — parameters are
    /// scalars.
    pub fn to_value(&self) -> Result<Value, String> {
        match self {
            Json::Null => Ok(Value::Null),
            Json::Bool(b) => Ok(Value::Integer(*b as i64)),
            Json::Int(i) => Ok(Value::Integer(*i)),
            Json::Real(f) => {
                if f.is_finite() {
                    Ok(Value::Real(*f))
                } else {
                    Ok(Value::Null)
                }
            }
            Json::Str(s) => Ok(Value::Text(s.clone().into())),
            Json::Object(pairs) => {
                if pairs.len() == 1 && pairs[0].0 == "blob" {
                    let hex = pairs[0].1.as_str().ok_or("blob tag must be a string")?;
                    let bytes = decode_hex(hex).ok_or("blob tag must be hex")?;
                    Ok(Value::Blob(bytes))
                } else {
                    Err("unsupported parameter shape (object)".to_string())
                }
            }
            Json::Array(_) => Err("unsupported parameter shape (array)".to_string()),
        }
    }

    /// Convert an engine [`Value`] to the wire form. See the module docs
    /// for the blob tag and the non-finite-REAL rule.
    pub fn from_value(v: &Value) -> Json {
        match v {
            Value::Null => Json::Null,
            Value::Integer(i) => Json::Int(*i),
            Value::Real(f) => {
                if f.is_finite() {
                    Json::Real(*f)
                } else {
                    Json::Null
                }
            }
            Value::Text(s) => Json::Str(s.as_str().to_string()),
            Value::Blob(b) => Json::Object(vec![("blob".to_string(), Json::Str(encode_hex(b)))]),
        }
    }

    /// Serialize to compact JSON. Strings are escaped per RFC 8259
    /// (control characters as `\u00XX`, the short escapes for `"` `\`
    /// and the whitespace controls).
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => out.push_str(&i.to_string()),
            Json::Real(f) => {
                if f.is_finite() {
                    // Rust's Display for f64 is the shortest form that
                    // round-trips, and every finite value it prints is a
                    // valid JSON number (exponent form included).
                    out.push_str(&format!("{}", f));
                    // Keep the number shape a JSON *real* (Rust prints
                    // "5" for 5.0; either is valid, keep integers of
                    // Real type distinguishable is unnecessary — 5 == 5.0
                    // for JSON parsers).
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_escaped(out, s),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escape a string as a JSON string literal (with surrounding quotes).
fn write_escaped(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    depth: usize,
}

const MAX_DEPTH: usize = 64;

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.pos))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        if self.depth >= MAX_DEPTH {
            return Err("nesting too deep".to_string());
        }
        match self.peek() {
            None => Err("unexpected end of input".to_string()),
            Some(b'{') => {
                self.depth += 1;
                let v = self.object();
                self.depth -= 1;
                v
            }
            Some(b'[') => {
                self.depth += 1;
                let v = self.array();
                self.depth -= 1;
                v
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(c) => Err(format!(
                "unexpected character '{}' at byte {}",
                c as char, self.pos
            )),
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(pairs));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'u' => {
                            // Standard \uXXXX (possibly followed by a
                            // surrogate pair) or Rust-style \u{XXXX}.
                            if self.peek() == Some(b'{') {
                                self.pos += 1;
                                let mut hex = String::new();
                                while let Some(h) = self.peek() {
                                    if h == b'}' {
                                        break;
                                    }
                                    hex.push(h as char);
                                    self.pos += 1;
                                }
                                self.expect(b'}')?;
                                let cp = u32::from_str_radix(&hex, 16)
                                    .map_err(|_| "bad \\u{...} escape".to_string())?;
                                out.push(char::from_u32(cp).ok_or("bad code point".to_string())?);
                            } else {
                                let hi = self.hex4()?;
                                let cp = if (0xD800..0xDC00).contains(&hi) {
                                    // Surrogate pair: \uXXXX\uXXXX
                                    if self.b[self.pos..].starts_with(b"\\u") {
                                        self.pos += 2;
                                        let lo = self.hex4()?;
                                        let combined =
                                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        char::from_u32(combined)
                                            .ok_or("bad surrogate pair".to_string())?
                                    } else {
                                        // Lone surrogate: replace (the
                                        // standard lossy fallback).
                                        '\u{fffd}'
                                    }
                                } else {
                                    char::from_u32(hi).ok_or("bad code point".to_string())?
                                };
                                out.push(cp);
                            }
                        }
                        _ => return Err("bad escape".to_string()),
                    }
                }
                c if c < 0x20 => return Err("control character in string".to_string()),
                c => {
                    // Multi-byte UTF-8: copy the whole sequence.
                    let len = utf8_len(c);
                    if len == 1 {
                        out.push(c as char);
                    } else {
                        let start = self.pos - 1;
                        let end = start + len;
                        if end > self.b.len() {
                            return Err("truncated UTF-8".to_string());
                        }
                        let s = std::str::from_utf8(&self.b[start..end])
                            .map_err(|_| "invalid UTF-8".to_string())?;
                        out.push_str(s);
                        self.pos = end;
                    }
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.b.len() {
            return Err("truncated \\u escape".to_string());
        }
        let s = std::str::from_utf8(&self.b[self.pos..self.pos + 4])
            .map_err(|_| "bad \\u escape".to_string())?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "bad \\u escape".to_string())?;
        self.pos += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let mut is_real = false;
        if self.peek() == Some(b'.') {
            is_real = true;
            self.pos += 1;
            let frac_start = self.pos;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            if self.pos == frac_start {
                return Err(format!("digit expected after '.' at byte {}", frac_start));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_real = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text =
            std::str::from_utf8(&self.b[start..self.pos]).map_err(|_| "bad number".to_string())?;
        if text.is_empty() || text == "-" {
            return Err(format!("bad number at byte {}", start));
        }
        if !is_real {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Json::Int(i));
            }
        }
        text.parse::<f64>()
            .map(Json::Real)
            .map_err(|_| format!("bad number at byte {}", start))
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1, // invalid lead byte — treat as 1, error surfaces as invalid UTF-8
    }
}

/// Lowercase hex encoding (used by the blob tag and the auth protocol).
pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Hex decoding (lowercase or uppercase input, even length, no whitespace).
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let hi = hex_val(b[i])?;
        let lo = hex_val(b[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_basics() {
        let src = r#"{"sql":"SELECT 1","params":[null,42,3.5,"a,b","quote\"inside","nl\nline"]}"#;
        let v = Json::parse(src).unwrap();
        assert_eq!(v.get("sql").unwrap().as_str().unwrap(), "SELECT 1");
        let params = v.get("params").unwrap().as_array().unwrap();
        assert_eq!(params.len(), 6);
        assert_eq!(params[4].as_str().unwrap(), "quote\"inside");
        // The comma inside a string must NOT split fields (the bug the old
        // hand-rolled array splitter had).
        assert_eq!(params[3].as_str().unwrap(), "a,b");
        // Re-serialize and re-parse.
        let out = v.to_json();
        let v2 = Json::parse(&out).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn unicode_and_escapes() {
        let v = Json::parse(r#""é😀 ok""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "é😀 ok");
        // Rust-style escape accepted.
        let v = Json::parse(r#""\u{41}\u{1f}""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "A\u{1f}");
        // Surrogate pair.
        let v = Json::parse(r#""😀""#).unwrap();
        assert_eq!(v.as_str().unwrap(), "😀");
    }

    #[test]
    fn numbers() {
        assert_eq!(Json::parse("0").unwrap(), Json::Int(0));
        assert_eq!(Json::parse("-17").unwrap(), Json::Int(-17));
        assert_eq!(
            Json::parse("9223372036854775807").unwrap(),
            Json::Int(i64::MAX)
        );
        // i64 overflow falls back to REAL (like SQLite's integer range).
        assert!(matches!(
            Json::parse("9223372036854775808").unwrap(),
            Json::Real(_)
        ));
        assert!(
            matches!(Json::parse("1.5e3").unwrap(), Json::Real(f) if (f - 1500.0).abs() < 1e-9)
        );
        assert!(Json::parse("1.").is_err());
        assert!(Json::parse("--1").is_err());
        assert!(Json::parse("01x").is_err()); // trailing data
    }

    #[test]
    fn nesting_limit_and_shapes() {
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(Json::parse(&deep).is_err());
        assert!(Json::parse("{\"a\":1,}").is_err());
        assert!(Json::parse("[1,]").is_err());
        assert!(Json::parse("").is_err());
        assert_eq!(
            Json::parse(" { \"k\" : [ true , false ] } ")
                .unwrap()
                .get("k")
                .unwrap()
                .as_array()
                .unwrap()[0],
            Json::Bool(true)
        );
    }

    #[test]
    fn value_conversion() {
        let j = Json::from_value(&Value::Blob(vec![0xde, 0xad]));
        assert_eq!(j.to_json(), r#"{"blob":"dead"}"#);
        assert_eq!(j.to_value().unwrap(), Value::Blob(vec![0xde, 0xad]));
        assert_eq!(Json::from_value(&Value::Real(f64::NAN)).to_json(), "null");
        assert_eq!(
            Json::from_value(&Value::Text("hi".into()))
                .to_value()
                .unwrap(),
            Value::Text("hi".into())
        );
        assert_eq!(
            Json::from_value(&Value::Integer(7)).to_value().unwrap(),
            Value::Integer(7)
        );
        assert_eq!(Json::Bool(false).to_value().unwrap(), Value::Integer(0));
        assert!(Json::Array(vec![Json::Int(1)]).to_value().is_err());
    }

    #[test]
    fn hex_helpers() {
        assert_eq!(encode_hex(&[0x00, 0xff, 0x10]), "00ff10");
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(decode_hex("00FF10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert!(decode_hex("0").is_none());
        assert!(decode_hex("zz").is_none());
    }
}
