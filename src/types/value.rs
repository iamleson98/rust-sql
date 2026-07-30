//! SQL value types and row representation.
//!
//! The engine uses a tagged-union `Value` enum, similar to SQLite's
//! dynamic typing (with a few concessions for performance: integers
//! use `i64`, floats use `f64`, blobs are `Vec<u8>`).

use std::cmp::Ordering;
use std::fmt;

/// A single SQL value.
///
/// Ordering follows SQLite's type affinity rules:
/// NULL < INTEGER/REAL < TEXT < BLOB
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    /// SQL type affinity for this value.
    pub fn type_affinity(&self) -> Affinity {
        match self {
            Value::Null => Affinity::None,
            Value::Integer(_) => Affinity::Integer,
            Value::Real(_) => Affinity::Real,
            Value::Text(_) => Affinity::Text,
            Value::Blob(_) => Affinity::Blob,
        }
    }

    /// True if the value is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Coerce to i64 using SQL semantics. NULL → 0, Real → truncated,
    /// Text → parsed (0 on failure), Blob → 0.
    pub fn as_integer(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Integer(i) => *i,
            Value::Real(f) => *f as i64,
            Value::Text(s) => s.trim().parse().unwrap_or(0),
            Value::Blob(_) => 0,
        }
    }

    /// Coerce to f64. NULL → 0.0, Integer → as f64, Text → parsed.
    pub fn as_real(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Integer(i) => *i as f64,
            Value::Real(f) => *f,
            Value::Text(s) => s.trim().parse().unwrap_or(0.0),
            Value::Blob(b) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
        }
    }

    /// Coerce to text.
    pub fn as_text(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Integer(i) => i.to_string(),
            Value::Real(f) => format_real(*f),
            Value::Text(s) => s.clone(),
            Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        }
    }

    /// Returns the byte length of the value as SQLite's `length()` function would.
    pub fn length(&self) -> i64 {
        match self {
            Value::Null => 0,
            Value::Integer(_) | Value::Real(_) => 8,
            Value::Text(s) => s.chars().count() as i64,
            Value::Blob(b) => b.len() as i64,
        }
    }

    /// True if value is truthy (non-zero, non-null, non-empty).
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Integer(i) => *i != 0,
            Value::Real(f) => *f != 0.0,
            Value::Text(s) => {
                // SQLite: numeric strings are parsed; "0" is false, anything else true.
                match s.trim().parse::<f64>() {
                    Ok(f) => f != 0.0,
                    Err(_) => !s.is_empty(),
                }
            }
            Value::Blob(b) => !b.is_empty(),
        }
    }

    /// Concatenate two values as text (SQLite `||` operator).
    pub fn concat(&self, other: &Value) -> Value {
        if self.is_null() || other.is_null() {
            return Value::Null;
        }
        Value::Text(format!("{}{}", self.as_text(), other.as_text()))
    }

    /// Encode the value for B+tree storage (compact binary form).
    /// Format: 1 byte tag + payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Value::Null => out.push(0),
            Value::Integer(i) => {
                out.push(1);
                out.extend_from_slice(&i.to_le_bytes());
            }
            Value::Real(f) => {
                out.push(2);
                out.extend_from_slice(&f.to_le_bytes());
            }
            Value::Text(s) => {
                out.push(3);
                let bytes = s.as_bytes();
                out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                out.extend_from_slice(bytes);
            }
            Value::Blob(b) => {
                out.push(4);
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
        }
        out
    }

    /// Decode a value from bytes. Returns (value, bytes consumed).
    pub fn decode(buf: &[u8]) -> Result<(Value, usize), &'static str> {
        if buf.is_empty() {
            return Err("empty buffer");
        }
        let tag = buf[0];
        let rest = &buf[1..];
        match tag {
            0 => Ok((Value::Null, 1)),
            1 => {
                if rest.len() < 8 {
                    return Err("truncated integer");
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&rest[..8]);
                Ok((Value::Integer(i64::from_le_bytes(b)), 9))
            }
            2 => {
                if rest.len() < 8 {
                    return Err("truncated real");
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&rest[..8]);
                Ok((Value::Real(f64::from_le_bytes(b)), 9))
            }
            3 => {
                if rest.len() < 4 {
                    return Err("truncated text length");
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&rest[..4]);
                let len = u32::from_le_bytes(b) as usize;
                if rest.len() < 4 + len {
                    return Err("truncated text body");
                }
                let s = std::str::from_utf8(&rest[4..4 + len])
                    .map_err(|_| "invalid utf8 in text")?
                    .to_string();
                Ok((Value::Text(s), 5 + len))
            }
            4 => {
                if rest.len() < 4 {
                    return Err("truncated blob length");
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&rest[..4]);
                let len = u32::from_le_bytes(b) as usize;
                if rest.len() < 4 + len {
                    return Err("truncated blob body");
                }
                Ok((Value::Blob(rest[4..4 + len].to_vec()), 5 + len))
            }
            _ => Err("unknown value tag"),
        }
    }
}

/// SQL type affinity (used in CREATE TABLE).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Real,
    Text,
    Blob,
    /// No affinity (NULL or expression result).
    None,
}

impl Affinity {
    /// SQLite's affinity rules: a column declared INTEGER gets INTEGER affinity,
    /// REAL/FLOAT/DOUBLE → REAL, CHAR/CLOB/TEXT → TEXT, BLOB or no type → BLOB.
    pub fn from_declared_type(decl: &str) -> Affinity {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            Affinity::Integer
        } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
            Affinity::Text
        } else if d.contains("BLOB") || d.is_empty() {
            Affinity::Blob
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Blob
        }
    }

    /// Apply affinity to a value, coercing if necessary.
    pub fn coerce(&self, v: Value) -> Value {
        match (*self, v) {
            (Affinity::Integer, Value::Integer(i)) => Value::Integer(i),
            (Affinity::Integer, Value::Real(f)) => Value::Integer(f as i64),
            (Affinity::Integer, Value::Text(s)) => match s.trim().parse::<i64>() {
                Ok(i) => Value::Integer(i),
                Err(_) => Value::Real(s.trim().parse::<f64>().unwrap_or(0.0)),
            },
            (Affinity::Integer, Value::Blob(b)) => Value::Integer(
                std::str::from_utf8(&b).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            ),
            (Affinity::Integer, Value::Null) => Value::Null,

            (Affinity::Real, Value::Integer(i)) => Value::Real(i as f64),
            (Affinity::Real, Value::Real(f)) => Value::Real(f),
            (Affinity::Real, Value::Text(s)) => {
                Value::Real(s.trim().parse::<f64>().unwrap_or(0.0))
            }
            (Affinity::Real, Value::Blob(b)) => Value::Real(
                std::str::from_utf8(&b).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0.0),
            ),
            (Affinity::Real, Value::Null) => Value::Null,

            (Affinity::Text, Value::Integer(i)) => Value::Text(i.to_string()),
            (Affinity::Text, Value::Real(f)) => Value::Text(format_real(f)),
            (Affinity::Text, Value::Text(s)) => Value::Text(s),
            (Affinity::Text, Value::Blob(b)) => Value::Text(String::from_utf8_lossy(&b).into_owned()),
            (Affinity::Text, Value::Null) => Value::Null,

            // BLOB and None: leave as-is
            (_, v) => v,
        }
    }
}

/// SQL ordering semantics: NULL sorts first.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        use Value::*;
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Less,
            (_, Null) => Ordering::Greater,
            (Integer(a), Integer(b)) => a.cmp(b),
            (Real(a), Real(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Integer(a), Real(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
            (Real(a), Integer(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
            (Integer(_) | Real(_), Text(_) | Blob(_)) => Ordering::Less,
            (Text(_) | Blob(_), Integer(_) | Real(_)) => Ordering::Greater,
            (Text(a), Text(b)) => a.cmp(b),
            (Blob(a), Blob(b)) => a.cmp(b),
            (Text(_), Blob(_)) => Ordering::Less,
            (Blob(_), Text(_)) => Ordering::Greater,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Real(x) => write!(f, "{}", format_real(*x)),
            Value::Text(s) => write!(f, "{}", s),
            Value::Blob(b) => write!(f, "{}", String::from_utf8_lossy(b)),
        }
    }
}

/// Format a real number with SQLite-style rounding (up to 15 significant digits).
pub fn format_real(f: f64) -> String {
    if f.is_nan() {
        return "".to_string();
    }
    if f == 0.0 {
        return "0.0".to_string();
    }
    // Use the shortest round-trippable representation, like SQLite.
    for digits in (1..=15).rev() {
        let candidate = format!("{:.*}", digits, f);
        if let Ok(parsed) = candidate.parse::<f64>() {
            if parsed == f {
                return normalize_real_string(candidate);
            }
        }
    }
    normalize_real_string(format!("{}", f))
}

fn normalize_real_string(s: String) -> String {
    // Ensure there's always a decimal point (SQLite quirk).
    if !s.contains('.') && !s.contains('e') && !s.contains("inf") {
        format!("{}.0", s)
    } else {
        s
    }
}

/// A row is an ordered sequence of values.
pub type Row = Vec<Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_ordering() {
        assert!(Value::Null < Value::Integer(0));
        assert!(Value::Integer(5) < Value::Integer(10));
        assert!(Value::Integer(5) < Value::Real(5.5));
        assert!(Value::Real(5.5) < Value::Text("a".to_string()));
        assert!(Value::Text("a".to_string()) < Value::Text("b".to_string()));
        assert!(Value::Text("z".to_string()) < Value::Blob(b"z".to_vec()));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let values = vec![
            Value::Null,
            Value::Integer(42),
            Value::Integer(-1_000_000),
            Value::Real(3.14159),
            Value::Text("hello".to_string()),
            Value::Text("unicode: 你好".to_string()),
            Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ];
        for v in values {
            let encoded = v.encode();
            let (decoded, n) = Value::decode(&encoded).unwrap();
            assert_eq!(n, encoded.len());
            assert_eq!(v, decoded);
        }
    }

    #[test]
    fn affinity_coercion() {
        assert_eq!(
            Affinity::Integer.coerce(Value::Text("42".into())),
            Value::Integer(42)
        );
        assert_eq!(
            Affinity::Real.coerce(Value::Integer(7)),
            Value::Real(7.0)
        );
        assert_eq!(
            Affinity::Text.coerce(Value::Integer(7)),
            Value::Text("7".into())
        );
    }

    #[test]
    fn truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Integer(0).is_truthy());
        assert!(Value::Integer(1).is_truthy());
        assert!(!Value::Text("0".into()).is_truthy());
        assert!(Value::Text("1".into()).is_truthy());
        assert!(Value::Text("abc".into()).is_truthy());
    }
}
