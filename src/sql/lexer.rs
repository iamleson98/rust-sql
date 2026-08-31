//! SQL lexer: converts source text into a stream of tokens.
//!
//! The lexer is a simple hand-written state machine. It tracks line and
//! column for error messages. Keywords are case-insensitive; identifiers
//! are case-sensitive (except for double-quoted identifiers, which preserve
//! case but compare case-insensitively at the semantic layer, like SQLite).

use crate::error::{Error, Result};

/// A SQL token.
#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// A keyword (SELECT, FROM, WHERE, etc.). Points at the canonical
    /// uppercase spelling in the static `KEYWORDS` table — zero allocation.
    /// (Previously `Keyword(String)`, which heap-allocated on every keyword
    /// token; a simple INSERT statement produced 5+ keyword allocations
    /// before the parser even started.)
    /// An integer literal that overflows i64 but fits u64 (currently only
    /// 9223372036854775808 = 2^63 in practice). Needed so the PARSER can
    /// fold `-9223372036854775808` into the INTEGER i64::MIN — SQLite
    /// parses that as an integer, not a real.
    HugeInteger(u64),
    Keyword(&'static str),
    /// An unquoted identifier (e.g. `users`, `name`). Identifiers are stored
    /// as-is (case preserved).
    Ident(String),
    /// A double-quoted identifier (e.g. `"first name"`).
    QuotedIdent(String),
    /// An integer literal.
    Integer(i64),
    /// A floating-point literal.
    Float(f64),
    /// A single-quoted string literal.
    String(String),
    /// A blob literal of the form `x'0123456789abcdef'`.
    Blob(Vec<u8>),
    /// A parameter placeholder (`?`, `?1`, `:name`, `@name`, `$name`).
    Parameter(String),
    /// An operator (e.g. `=`, `<=`, `+`). Points at a static string —
    /// zero allocation (previously `Op(String)`, allocating per token).
    Op(&'static str),
    /// A punctuation character (e.g. `(`, `)`, `,`, `;`, `.`).
    Punct(char),
    /// End of input.
    Eof,
}

impl Token {
    pub fn is_keyword(&self, kw: &str) -> bool {
        matches!(self, Token::Keyword(s) if s.eq_ignore_ascii_case(kw))
    }

    pub fn is_punct(&self, c: char) -> bool {
        matches!(self, Token::Punct(p) if *p == c)
    }

    pub fn is_op(&self, s: &str) -> bool {
        matches!(self, Token::Op(o) if *o == s)
    }
}

/// A token with its position in the source.
#[derive(Clone, Debug)]
pub struct SpannedToken {
    pub token: Token,
    pub line: usize,
    pub col: usize,
}

impl SpannedToken {
    pub fn is_keyword(&self, kw: &str) -> bool {
        self.token.is_keyword(kw)
    }
    pub fn is_punct(&self, c: char) -> bool {
        self.token.is_punct(c)
    }
    pub fn is_op(&self, s: &str) -> bool {
        self.token.is_op(s)
    }
}

/// The lexer.
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
    /// Next anonymous-`?` index. Each `?` token (without explicit digits)
    /// consumes this number and increments the counter so that successive
    /// `?` placeholders refer to distinct parameters. This mirrors SQLite's
    /// semantics: `WHERE a = ? AND b = ?` binds `a` to the first param and
    /// `b` to the second, NOT both to the first param (which was the prior
    /// behavior and caused incorrect query results whenever more than one
    /// anonymous `?` appeared in a single statement).
    next_anon_param: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            next_anon_param: 0,
        }
    }

    /// Tokenize the entire source.
    pub fn tokenize(mut self) -> Result<Vec<SpannedToken>> {
        let mut out = Vec::new();
        loop {
            self.skip_whitespace_and_comments()?;
            if self.pos >= self.src.len() {
                out.push(SpannedToken { token: Token::Eof, line: self.line, col: self.col });
                break;
            }
            let line = self.line;
            let col = self.col;
            let tok = self.next_token()?;
            out.push(SpannedToken { token: tok, line, col });
        }
        Ok(out)
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<()> {
        loop {
            if self.pos >= self.src.len() {
                return Ok(());
            }
            let c = self.src[self.pos];
            if c == b' ' || c == b'\t' || c == b'\r' {
                self.advance();
            } else if c == b'\n' {
                self.line += 1;
                self.col = 1;
                self.pos += 1;
            } else if c == b'-' && self.peek(1) == Some(b'-') {
                // Line comment
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.advance();
                }
            } else if c == b'/' && self.peek(1) == Some(b'*') {
                // Block comment
                self.advance();
                self.advance();
                while self.pos < self.src.len() {
                    if self.src[self.pos] == b'*' && self.peek(1) == Some(b'/') {
                        self.advance();
                        self.advance();
                        break;
                    }
                    if self.src[self.pos] == b'\n' {
                        self.line += 1;
                        self.col = 1;
                        self.pos += 1;
                    } else {
                        self.advance();
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    fn next_token(&mut self) -> Result<Token> {
        let c = self.src[self.pos];
        // Blob literal — check BEFORE identifier (since 'x' would otherwise be lexed as ident).
        if (c == b'x' || c == b'X') && self.peek(1) == Some(b'\'') {
            return self.lex_blob();
        }
        // Identifier or keyword (starts with letter or underscore).
        if c.is_ascii_alphabetic() || c == b'_' {
            return self.lex_ident_or_keyword();
        }
        // Numeric literal
        if c.is_ascii_digit() || (c == b'.' && self.peek(1).map(|d| d.is_ascii_digit()).unwrap_or(false)) {
            return self.lex_number();
        }
        // String literal
        if c == b'\'' {
            return self.lex_string();
        }
        // Quoted identifier
        if c == b'"' {
            return self.lex_quoted_ident();
        }
        // Parameter placeholder
        if c == b'?' || c == b':' || c == b'@' || c == b'$' {
            return self.lex_parameter();
        }
        // Punctuation
        if matches!(c, b'(' | b')' | b',' | b';' | b'.') {
            self.advance();
            return Ok(Token::Punct(c as char));
        }
        // Operators (multi-char first) — zero-allocation byte matching.
        if let Some(op) = self.try_multi_char_op() {
            return Ok(Token::Op(op));
        }
        // Single-char operator
        if matches!(c, b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'|' | b'&' | b'~' | b'^') {
            self.advance();
            let op: &'static str = match c {
                b'+' => "+",
                b'-' => "-",
                b'*' => "*",
                b'/' => "/",
                b'%' => "%",
                b'=' => "=",
                b'<' => "<",
                b'>' => ">",
                b'!' => "!",
                b'|' => "|",
                b'&' => "&",
                b'~' => "~",
                b'^' => "^",
                _ => unreachable!(),
            };
            return Ok(Token::Op(op));
        }
        Err(Error::lex(self.line, self.col, format!("unexpected character '{}'", c as char)))
    }

    fn lex_ident_or_keyword(&mut self) -> Result<Token> {
        let start = self.pos;
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                self.advance();
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).map_err(|_| {
            Error::lex(self.line, self.col, "invalid utf8 in identifier")
        })?;
        // Zero-allocation keyword recognition: binary search over the sorted
        // static table, comparing case-insensitively. Previously this did
        // `s.to_ascii_uppercase()` (a String allocation per identifier!) plus
        // a linear scan over 144 keywords (~900 string compares per INSERT
        // statement). Now: ~7 short memcmps, no allocation.
        if let Some(kw) = keyword_lookup(s) {
            Ok(Token::Keyword(kw))
        } else {
            Ok(Token::Ident(s.to_string()))
        }
    }

    fn lex_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut is_float = false;
        // Hex literal: 0x... (check FIRST, before consuming integer part).
        if self.peek(0) == Some(b'0') && matches!(self.peek(1), Some(b'x') | Some(b'X')) {
            self.advance();
            self.advance();
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_hexdigit() {
                self.advance();
            }
            let s = std::str::from_utf8(&self.src[start + 2..self.pos]).unwrap();
            return Ok(Token::Integer(i64::from_str_radix(s, 16).map_err(|_| {
                Error::lex(self.line, self.col, "invalid hex literal")
            })?));
        }
        // Integer part
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
            self.advance();
        }
        // Fractional part
        if self.peek(0) == Some(b'.') {
            is_float = true;
            self.advance();
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                self.advance();
            }
        }
        // Exponent
        if matches!(self.peek(0), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.advance();
            if matches!(self.peek(0), Some(b'+') | Some(b'-')) {
                self.advance();
            }
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                self.advance();
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        if is_float {
            Ok(Token::Float(s.parse().map_err(|_| {
                Error::lex(self.line, self.col, "invalid float literal")
            })?))
        } else {
            match s.parse::<i64>() {
                Ok(v) => Ok(Token::Integer(v)),
                // Out-of-i64-range integer literal. If it still fits u64,
                // keep it exact so the parser can fold `-9223372036854775808`
                // into INTEGER i64::MIN (SQLite semantics); beyond u64 or
                // with float syntax it becomes a REAL (SQLite:
                // `SELECT 9223372036854775808` is 9.223372036854776e18).
                Err(_) => match s.parse::<u64>() {
                    Ok(u) => Ok(Token::HugeInteger(u)),
                    Err(_) => Ok(Token::Float(s.parse::<f64>().map_err(
                        |_| Error::lex(self.line, self.col, "invalid integer literal"),
                    )?)),
                },
            }
        }
    }

    fn lex_string(&mut self) -> Result<Token> {
        self.advance(); // skip opening quote
        // Collect RAW BYTES, then decode as UTF-8 at the end. The previous
        // implementation pushed each byte with `c as char`, which passes
        // every byte through Latin-1 — multi-byte UTF-8 text like 'héllo'
        // was silently mangled into "hÃ©llo".
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(Error::lex(self.line, self.col, "unterminated string literal"));
            }
            let c = self.src[self.pos];
            if c == b'\'' {
                // Check for escaped quote ('')
                if self.peek(1) == Some(b'\'') {
                    bytes.push(b'\'');
                    self.advance();
                    self.advance();
                } else {
                    self.advance();
                    break;
                }
            } else {
                bytes.push(c);
                if c == b'\n' {
                    self.line += 1;
                    self.col = 1;
                    self.pos += 1;
                } else {
                    self.advance();
                }
            }
        }
        let s = String::from_utf8(bytes).map_err(|_| {
            Error::lex(self.line, self.col, "invalid utf8 in string literal")
        })?;
        Ok(Token::String(s))
    }

    fn lex_blob(&mut self) -> Result<Token> {
        self.advance(); // skip 'x'
        self.advance(); // skip opening quote
        let start = self.pos;
        while self.pos < self.src.len() && self.src[self.pos] != b'\'' {
            self.advance();
        }
        if self.pos >= self.src.len() {
            return Err(Error::lex(self.line, self.col, "unterminated blob literal"));
        }
        // Operate purely on BYTES: blob-literal content may be any byte
        // sequence (fuzzed input, 0xFF junk), so it must never be routed
        // through &str — `&hex[i..i+2]` panics when i+2 splits a multi-byte
        // UTF-8 char, and `from_utf8().unwrap()` panics on invalid UTF-8.
        let hex_bytes = &self.src[start..self.pos];
        self.advance(); // skip closing quote
        if hex_bytes.len() % 2 != 0 {
            return Err(Error::lex(self.line, self.col, "blob literal must have even number of hex digits"));
        }
        let mut bytes = Vec::with_capacity(hex_bytes.len() / 2);
        for i in (0..hex_bytes.len()).step_by(2) {
            let hi = hex_byte_value(hex_bytes[i]);
            let lo = hex_byte_value(hex_bytes[i + 1]);
            match (hi, lo) {
                (Some(h), Some(l)) => bytes.push((h << 4) | l),
                _ => {
                    return Err(Error::lex(
                        self.line,
                        self.col,
                        "invalid hex digit in blob literal",
                    ))
                }
            }
        }
        Ok(Token::Blob(bytes))
    }

    fn lex_quoted_ident(&mut self) -> Result<Token> {
        self.advance(); // skip opening quote
        // Raw bytes + UTF-8 decode (see lex_string — same Latin-1 bug fix).
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(Error::lex(self.line, self.col, "unterminated quoted identifier"));
            }
            let c = self.src[self.pos];
            if c == b'"' {
                if self.peek(1) == Some(b'"') {
                    bytes.push(b'"');
                    self.advance();
                    self.advance();
                } else {
                    self.advance();
                    break;
                }
            } else {
                bytes.push(c);
                self.advance();
            }
        }
        let s = String::from_utf8(bytes).map_err(|_| {
            Error::lex(self.line, self.col, "invalid utf8 in quoted identifier")
        })?;
        Ok(Token::QuotedIdent(s))
    }

    fn lex_parameter(&mut self) -> Result<Token> {
        let prefix = self.src[self.pos] as char;
        self.advance();
        let mut name = String::new();
        // For `?` followed by digits: `?1`, `?2`, etc.
        // For `:name`, `@name`, `$name`: identifier chars.
        if prefix == '?' {
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                name.push(self.src[self.pos] as char);
                self.advance();
            }
            if name.is_empty() {
                // Anonymous `?` — assign the next incrementing index so that
                // successive `?` tokens in the same statement bind to distinct
                // parameters. The name is the decimal string of the index
                // (e.g. "0", "1", "2"), which `Database::execute` and
                // `ExecContext::bind` look up by string key.
                let idx = self.next_anon_param;
                self.next_anon_param += 1;
                name = idx.to_string();
            }
        } else {
            name.push(prefix);
            while self.pos < self.src.len() {
                let c = self.src[self.pos];
                if c.is_ascii_alphanumeric() || c == b'_' {
                    name.push(c as char);
                    self.advance();
                } else {
                    break;
                }
            }
        }
        Ok(Token::Parameter(name))
    }

    fn try_multi_char_op(&mut self) -> Option<&'static str> {
        // Zero-allocation operator matching on raw bytes. The previous
        // implementation called `format!("{}{}{}", ...)` for the 3-char
        // probe AND `format!("{}{}", ...)` for the 2-char probe on EVERY
        // operator token — 2 heap allocations just to lex `=` or `+`.
        // (The 3-char block was dead code: no 3-char operator was ever
        // returned.) Now: direct byte comparison, no allocation.
        let a = *self.src.get(self.pos)?;
        let b = *self.src.get(self.pos + 1)?;
        let op: &'static str = match (a, b) {
            (b'<', b'=') => "<=",
            (b'>', b'=') => ">=",
            (b'!', b'=') => "!=",
            (b'<', b'>') => "<>",
            (b'=', b'=') => "==",
            (b'|', b'|') => "||",
            (b'<', b'<') => "<<",
            (b'>', b'>') => ">>",
            _ => return None,
        };
        self.advance();
        self.advance();
        Some(op)
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn advance(&mut self) {
        self.pos += 1;
        self.col += 1;
    }
}

/// Value of a single ASCII hex digit (0-9, a-f, A-F) as a nibble, or None
/// for any other byte. Byte-oriented on purpose: blob literals must be
/// validated byte-by-byte so arbitrary (fuzzed) input can never reach a
/// `&str` slice that could split a multi-byte UTF-8 sequence.
#[inline]
fn hex_byte_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Returns true if `s` is a SQL keyword (case-insensitive).
/// Zero-allocation (previously: `KEYWORDS.contains(&s.to_ascii_uppercase())`
/// — one String allocation + a 144-entry linear scan per call).
pub fn is_keyword(s: &str) -> bool {
    keyword_lookup(s).is_some()
}

/// Case-insensitive lookup of `s` in the sorted static keyword table.
/// Returns the canonical uppercase spelling. ~log2(144) ≈ 8 short compares,
/// zero allocation.
pub fn keyword_lookup(s: &str) -> Option<&'static str> {
    let sb = s.as_bytes();
    KEYWORDS
        .binary_search_by(|kw| {
            let kb = kw.as_bytes();
            // `& 0xDF` uppercases ASCII letters and leaves `_` untouched.
            // Keywords contain only A-Z and `_`, so this is the identity on
            // the array side — matching its plain byte sort order — while
            // case-folding the probe side.
            let n = kb.len().min(sb.len());
            for i in 0..n {
                let a = kb[i] & 0xDF;
                let b = sb[i] & 0xDF;
                if a != b {
                    return a.cmp(&b);
                }
            }
            kb.len().cmp(&sb.len())
        })
        .ok()
        .map(|i| KEYWORDS[i])
}

/// The set of SQL keywords recognized by the lexer. MUST stay sorted by
/// raw byte order (binary search in `keyword_lookup` depends on it).
pub const KEYWORDS: &[&str] = &[
    "ABORT", "ACTION", "ADD", "AFTER", "ALL", "ALTER", "ANALYZE", "AND",
    "AS", "ASC", "ATTACH", "AUTOINCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BY",
    "CASCADE", "CASE", "CAST", "CHECK", "COLLATE", "COLUMN", "COMMIT", "CONFLICT",
    "CONSTRAINT", "CREATE", "CROSS", "CURRENT", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "DATABASE",
    "DEFAULT", "DEFERRABLE", "DEFERRED", "DELETE", "DESC", "DETACH", "DISTINCT", "DO",
    "DROP", "EACH", "ELSE", "END", "ESCAPE", "EXCEPT", "EXCLUDE", "EXCLUSIVE",
    "EXISTS", "EXPLAIN", "FAIL", "FILTER", "FOLLOWING", "FOR", "FOREIGN", "FROM",
    "FULL", "GLOB", "GROUP", "GROUPS", "HAVING", "IF", "IGNORE", "IMMEDIATE",
    "IN", "INDEX", "INDEXED", "INITIALLY", "INNER", "INSERT", "INSTEAD", "INTERSECT",
    "INTO", "IS", "ISNULL", "JOIN", "KEY", "LEFT", "LIKE", "LIMIT",
    "MATCH", "MATERIALIZED", "NATURAL", "NO", "NOT", "NOTHING", "NOTNULL", "NULL",
    "NULLS", "OF", "OFFSET", "ON", "OR", "ORDER", "OTHERS", "OUTER",
    "OVER", "PARTITION", "PLAN", "PRAGMA", "PRECEDING", "PRIMARY", "QUERY", "RAISE",
    "RANGE", "RECURSIVE", "REFERENCES", "REGEXP", "REINDEX", "RELEASE", "RENAME", "REPLACE",
    "RESTRICT", "RETURNING", "RIGHT", "ROLLBACK", "ROW", "ROWS", "SAVEPOINT", "SELECT",
    "SET", "STORED", "TABLE", "TEMP", "TEMPORARY", "THEN", "TIES", "TO",
    "TRANSACTION", "TRIGGER", "UNBOUNDED", "UNION", "UNIQUE", "UPDATE", "USING", "VACUUM",
    "VALUES", "VIEW", "VIRTUAL", "WHEN", "WHERE", "WINDOW", "WITH", "WITHOUT",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Token> {
        let toks = Lexer::new(src).tokenize().unwrap();
        toks.into_iter().map(|s| s.token).collect()
    }

    #[test]
    fn keywords_and_idents() {
        let toks = lex("SELECT name FROM users");
        assert_eq!(toks[0], Token::Keyword("SELECT"));
        assert_eq!(toks[1], Token::Ident("name".into()));
        assert_eq!(toks[2], Token::Keyword("FROM"));
        assert_eq!(toks[3], Token::Ident("users".into()));
        assert_eq!(toks[4], Token::Eof);
    }

    #[test]
    fn numbers() {
        let toks = lex("42 1.25 0x1F 1.5e-3");
        assert_eq!(toks[0], Token::Integer(42));
        assert_eq!(toks[1], Token::Float(1.25));
        assert_eq!(toks[2], Token::Integer(31));
        assert_eq!(toks[3], Token::Float(0.0015));
    }

    #[test]
    fn strings_and_blobs() {
        let toks = lex("'hello' 'it''s' x'CAFEBABE'");
        assert_eq!(toks[0], Token::String("hello".into()));
        assert_eq!(toks[1], Token::String("it's".into()));
        assert_eq!(toks[2], Token::Blob(vec![0xCA, 0xFE, 0xBA, 0xBE]));
    }

    #[test]
    fn operators() {
        let toks = lex("a <= b >= c != d <> e == f || g << h >> i");
        assert_eq!(toks[1], Token::Op("<="));
        assert_eq!(toks[3], Token::Op(">="));
        assert_eq!(toks[5], Token::Op("!="));
        assert_eq!(toks[7], Token::Op("<>"));
        assert_eq!(toks[9], Token::Op("=="));
        assert_eq!(toks[11], Token::Op("||"));
        assert_eq!(toks[13], Token::Op("<<"));
        assert_eq!(toks[15], Token::Op(">>"));
    }

    #[test]
    fn comments() {
        let toks = lex("SELECT 1 -- comment\n+ /* block */ 2");
        assert_eq!(toks[0], Token::Keyword("SELECT"));
        assert_eq!(toks[1], Token::Integer(1));
        assert_eq!(toks[2], Token::Op("+"));
        assert_eq!(toks[3], Token::Integer(2));
    }

    #[test]
    fn parameters() {
        let toks = lex("? ?1 :name @col $var");
        assert_eq!(toks[0], Token::Parameter("0".into()));
        assert_eq!(toks[1], Token::Parameter("1".into()));
        assert_eq!(toks[2], Token::Parameter(":name".into()));
        assert_eq!(toks[3], Token::Parameter("@col".into()));
        assert_eq!(toks[4], Token::Parameter("$var".into()));
    }

    #[test]
    fn multiple_anonymous_placeholders_get_distinct_indices() {
        // Regression: previously every anonymous `?` lexed to Parameter("0"),
        // so `WHERE a = ? AND b = ?` bound both predicates to the first
        // parameter, silently masking the second value. Now each `?` gets
        // the next incrementing index.
        let toks = lex("? ? ?");
        assert_eq!(toks[0], Token::Parameter("0".into()));
        assert_eq!(toks[1], Token::Parameter("1".into()));
        assert_eq!(toks[2], Token::Parameter("2".into()));

        // Mixed `?` and `?N` — explicit N does NOT advance the anonymous
        // counter, so a later `?` continues from the last assigned index.
        let toks = lex("? ?1 ?");
        assert_eq!(toks[0], Token::Parameter("0".into()));
        assert_eq!(toks[1], Token::Parameter("1".into()));
        assert_eq!(toks[2], Token::Parameter("1".into()));
    }
}
