//! pg_trgm-borrowed trigram functions: `similarity`, `word_similarity`,
//! `show_trgm`.
//!
//! Trigram extraction follows pg_trgm: downcase, split into alphanumeric
//! words, pad each word with two leading spaces and one trailing space,
//! take every 3-character window, and keep the DISTINCT set.
//!
//! - `similarity(a, b)` = |A ∩ B| / |A ∪ B| (the Jaccard index of the two
//!   trigram sets — identical strings score 1.0, disjoint ones 0.0, the
//!   documented pg_trgm range).
//! - `word_similarity(a, b)` = the best Jaccard of `a`'s trigrams against
//!   any contiguous window of `b`'s words [simplified vs pg_trgm's
//!   positional alignment; windows are capped at 32 words per side, above
//!   which single words + the whole string are considered].
//! - `show_trgm(text)` = the trigram set as a JSON array of strings
//!   (PostgreSQL returns `text[]`; this engine surfaces arrays as JSON —
//!   the same convention as the JSON1 family).
//!
//! NULL-in-NULL-out for every argument (PostgreSQL semantics).

use crate::error::{Error, Result};
use crate::types::Value;
use std::collections::BTreeSet;

/// Extract the distinct trigram set of a text (pg_trgm rules).
fn trigrams(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let lower: String = text.to_lowercase();
    // Alphanumeric words only; everything else is a separator.
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut BTreeSet<String>| {
        if word.is_empty() {
            return;
        }
        // Pad: two leading spaces, one trailing space.
        let padded: Vec<char> = std::iter::repeat(' ')
            .take(2)
            .chain(word.chars())
            .chain(std::iter::once(' '))
            .collect();
        for i in 0..padded.len().saturating_sub(2) {
            let tri: String = padded[i..i + 3].iter().collect();
            out.insert(tri);
        }
        word.clear();
    };
    for c in lower.chars() {
        if c.is_alphanumeric() {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
        }
    }
    flush(&mut word, &mut out);
    out
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        return 1.0;
    }
    inter as f64 / union as f64
}

/// JSON string escaping for show_trgm's array rendering.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

const WORD_WINDOW_CAP: usize = 32;

/// Contiguous-word windows of `b` for word_similarity. Returns each
/// window's trigram set; above the cap, only single words and the whole
/// string are considered (documented simplification).
fn word_windows(text: &str) -> Vec<BTreeSet<String>> {
    let lower: String = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    if words.len() <= WORD_WINDOW_CAP {
        for start in 0..words.len() {
            for end in start..words.len() {
                let joined = words[start..=end].join(" ");
                out.push(trigrams(&joined));
            }
        }
    } else {
        for w in &words {
            out.push(trigrams(w));
        }
        out.push(trigrams(text));
    }
    out
}

fn similarity(a: &Value, b: &Value) -> Result<Option<Value>> {
    if a.is_null() || b.is_null() {
        return Ok(Some(Value::Null));
    }
    let ta = trigrams(&a.as_text());
    let tb = trigrams(&b.as_text());
    Ok(Some(Value::Real(jaccard(&ta, &tb))))
}

fn word_similarity(a: &Value, b: &Value) -> Result<Option<Value>> {
    if a.is_null() || b.is_null() {
        return Ok(Some(Value::Null));
    }
    let ta = trigrams(&a.as_text());
    let mut best = 0.0f64;
    for w in word_windows(&b.as_text()) {
        let s = jaccard(&ta, &w);
        if s > best {
            best = s;
        }
    }
    Ok(Some(Value::Real(best)))
}

fn show_trgm(v: &Value) -> Result<Option<Value>> {
    if v.is_null() {
        return Ok(Some(Value::Null));
    }
    let t = trigrams(&v.as_text());
    let body: Vec<String> = t
        .iter()
        .map(|s| format!("\"{}\"", json_escape(s)))
        .collect();
    Ok(Some(Value::Text(format!("[{}]", body.join(",")).into())))
}

/// Scalar dispatch entry, called from `expr.rs` after user functions.
/// Returns `Ok(None)` when `name` is not a trigram function.
pub fn call_trgm_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    match name {
        "similarity" => {
            if args.len() != 2 {
                return Err(Error::runtime("similarity(a, b) takes 2 arguments"));
            }
            similarity(&args[0], &args[1])
        }
        "word_similarity" => {
            if args.len() != 2 {
                return Err(Error::runtime("word_similarity(a, b) takes 2 arguments"));
            }
            word_similarity(&args[0], &args[1])
        }
        "show_trgm" => {
            if args.len() != 1 {
                return Err(Error::runtime("show_trgm(text) takes 1 argument"));
            }
            show_trgm(&args[0])
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn trigram_extraction() {
        // 'cat' -> '  cat ' -> 4 trigrams
        let g = trigrams("cat");
        let expect: BTreeSet<String> = ["  c", " ca", "cat", "at "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(g, expect);
        // punctuation splits words
        let g = trigrams("a-b");
        let expect: BTreeSet<String> = ["  a", " a ", "  b", " b "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(g, expect);
        // downcasing
        assert_eq!(trigrams("CAT"), trigrams("cat"));
        // empty
        assert!(trigrams("").is_empty());
        assert!(trigrams(" !!! ").is_empty());
    }

    #[test]
    fn similarity_bounds() {
        // identical -> 1
        let r = call_trgm_function("similarity", &[t("word"), t("word")])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Real(1.0));
        // disjoint -> 0
        let r = call_trgm_function("similarity", &[t("aaa"), t("zzz")])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Real(0.0));
        // partial in (0,1)
        let r = call_trgm_function("similarity", &[t("hello"), t("hallo")])
            .unwrap()
            .unwrap();
        if let Value::Real(f) = r {
            assert!(f > 0.0 && f < 1.0, "similarity(hello, hallo) = {}", f);
        } else {
            panic!("not a real");
        }
        // NULL
        let r = call_trgm_function("similarity", &[Value::Null, t("x")])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Null);
    }

    #[test]
    fn word_similarity_contains() {
        // 'word' inside a longer string should beat an unrelated one
        let inside = call_trgm_function("word_similarity", &[t("word"), t("a word here")])
            .unwrap()
            .unwrap();
        let outside = call_trgm_function("word_similarity", &[t("word"), t("zzzz unrelated")])
            .unwrap()
            .unwrap();
        let (Value::Real(a), Value::Real(b)) = (inside, outside) else {
            panic!("not reals");
        };
        assert!(a > b);
        assert!(a > 0.5);
        assert!(b < 0.2);
    }

    #[test]
    fn show_trgm_shape() {
        let r = call_trgm_function("show_trgm", &[t("cat")])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Text(r#"["  c"," ca","at ","cat"]"#.into()));
        // JSON escaping: trigram contents are always alphanumeric-plus-
        // spaces by construction (non-alnums are word separators), so no
        // escape can appear in practice — assert the invariant directly.
        let r = call_trgm_function("show_trgm", &[t(r#""q"#)])
            .unwrap()
            .unwrap();
        if let Value::Text(s) = r {
            assert_eq!(s.as_str(), r#"["  q"," q "]"#);
            assert!(!s.as_str().contains('\\'));
        }
        // NULL
        let r = call_trgm_function("show_trgm", &[Value::Null])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Null);
        // unknown name
        assert!(call_trgm_function("nope", &[]).unwrap().is_none());
    }
}
