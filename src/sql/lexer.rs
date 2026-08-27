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
    /// A keyword (SELECT, FROM, WHERE, etc.). Keywords are stored in uppercase.
    Keyword(String),
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
    /// An operator (e.g. `=`, `<=`, `+`).
    Op(String),
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
        matches!(self, Token::Op(o) if o == s)
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
        // Operators (multi-char first)
        if let Some(op) = self.try_multi_char_op() {
            return Ok(Token::Op(op));
        }
        // Single-char operator
        if matches!(c, b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'|' | b'&' | b'~' | b'^') {
            self.advance();
            return Ok(Token::Op((c as char).to_string()));
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
        if is_keyword(s) {
            Ok(Token::Keyword(s.to_ascii_uppercase()))
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
            Ok(Token::Integer(s.parse().map_err(|_| {
                Error::lex(self.line, self.col, "invalid integer literal")
            })?))
        }
    }

    fn lex_string(&mut self) -> Result<Token> {
        self.advance(); // skip opening quote
        let mut out = String::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(Error::lex(self.line, self.col, "unterminated string literal"));
            }
            let c = self.src[self.pos];
            if c == b'\'' {
                // Check for escaped quote ('')
                if self.peek(1) == Some(b'\'') {
                    out.push('\'');
                    self.advance();
                    self.advance();
                } else {
                    self.advance();
                    break;
                }
            } else {
                out.push(c as char);
                if c == b'\n' {
                    self.line += 1;
                    self.col = 1;
                } else {
                    self.advance();
                }
            }
        }
        Ok(Token::String(out))
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
        let hex = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        self.advance(); // skip closing quote
        if hex.len() % 2 != 0 {
            return Err(Error::lex(self.line, self.col, "blob literal must have even number of hex digits"));
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for i in (0..hex.len()).step_by(2) {
            let b = u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| {
                Error::lex(self.line, self.col, "invalid hex digit in blob literal")
            })?;
            bytes.push(b);
        }
        Ok(Token::Blob(bytes))
    }

    fn lex_quoted_ident(&mut self) -> Result<Token> {
        self.advance(); // skip opening quote
        let mut out = String::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(Error::lex(self.line, self.col, "unterminated quoted identifier"));
            }
            let c = self.src[self.pos];
            if c == b'"' {
                if self.peek(1) == Some(b'"') {
                    out.push('"');
                    self.advance();
                    self.advance();
                } else {
                    self.advance();
                    break;
                }
            } else {
                out.push(c as char);
                self.advance();
            }
        }
        Ok(Token::QuotedIdent(out))
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

    fn try_multi_char_op(&mut self) -> Option<String> {
        let two = if self.pos + 1 < self.src.len() {
            Some((self.src[self.pos] as char, self.src[self.pos + 1] as char))
        } else {
            None
        };
        let three = if self.pos + 2 < self.src.len() {
            Some((
                self.src[self.pos] as char,
                self.src[self.pos + 1] as char,
                self.src[self.pos + 2] as char,
            ))
        } else {
            None
        };
        // Three-char operators
        if let Some((a, b, c)) = three {
            let s = format!("{}{}{}", a, b, c);
            if matches!(s.as_str(), "<<" | ">>") {
                // 2-char, fall through
            } else if s == "!=" || s == "<=>" {
                // 2-char, fall through
            } else if s == "||>" || s == "|>>" {
                // not supported
            }
        }
        // Two-char operators
        if let Some((a, b)) = two {
            let s = format!("{}{}", a, b);
            match s.as_str() {
                "<=" | ">=" | "!=" | "<>" | "==" | "||" | "<<" | ">>" => {
                    self.advance();
                    self.advance();
                    return Some(s);
                }
                _ => {}
            }
        }
        None
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn advance(&mut self) {
        self.pos += 1;
        self.col += 1;
    }
}

/// Returns true if `s` is a SQL keyword (case-insensitive).
pub fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s.to_ascii_uppercase().as_str())
}

/// The set of SQL keywords recognized by the lexer.
pub const KEYWORDS: &[&str] = &[
    "ABORT", "ACTION", "ADD", "AFTER", "ALL", "ALTER", "ANALYZE", "AND", "AS", "ASC",
    "ATTACH", "AUTOINCREMENT", "BEFORE", "BEGIN", "BETWEEN", "BY", "CASCADE", "CASE",
    "CAST", "CHECK", "COLLATE", "COLUMN", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE",
    "CROSS", "CURRENT", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP", "DATABASE",
    "DEFAULT", "DEFERRABLE", "DEFERRED", "DELETE", "DESC", "DETACH", "DISTINCT", "DO",
    "DROP", "EACH", "ELSE", "END", "ESCAPE", "EXCEPT", "EXCLUSIVE", "EXCLUDE", "EXISTS",
    "EXPLAIN", "FAIL", "FILTER", "FOLLOWING", "FOR", "FOREIGN", "FROM", "FULL", "GLOB",
    "GROUP", "GROUPS", "HAVING", "IF", "IGNORE", "IMMEDIATE", "IN", "INDEX", "INDEXED",
    "INITIALLY", "INNER", "INSERT", "INSTEAD", "INTERSECT", "INTO", "IS", "ISNULL",
    "JOIN", "KEY", "LEFT", "LIKE", "LIMIT", "MATCH", "MATERIALIZED", "NATURAL", "NO",
    "NOT", "NOTHING", "NOTNULL", "NULL", "NULLS", "OF", "OFFSET", "ON", "OR", "ORDER",
    "OTHERS", "OUTER", "OVER", "PARTITION", "PLAN", "PRAGMA", "PRECEDING", "PRIMARY",
    "QUERY", "RAISE", "RANGE", "RECURSIVE", "REFERENCES", "REGEXP", "REINDEX", "RELEASE",
    "RENAME", "REPLACE", "RESTRICT", "RETURNING", "RIGHT", "ROLLBACK", "ROW", "ROWS",
    "SAVEPOINT", "SELECT", "SET", "STORED", "TABLE", "TEMP", "TEMPORARY", "TIES", "THEN",
    "TO", "TRANSACTION", "TRIGGER", "UNBOUNDED", "UNION", "UNIQUE", "UPDATE", "USING",
    "VACUUM", "VALUES", "VIEW", "VIRTUAL", "WHEN", "WHERE", "WINDOW", "WITH", "WITHOUT",
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
        assert_eq!(toks[0], Token::Keyword("SELECT".into()));
        assert_eq!(toks[1], Token::Ident("name".into()));
        assert_eq!(toks[2], Token::Keyword("FROM".into()));
        assert_eq!(toks[3], Token::Ident("users".into()));
        assert_eq!(toks[4], Token::Eof);
    }

    #[test]
    fn numbers() {
        let toks = lex("42 3.14 0x1F 1.5e-3");
        assert_eq!(toks[0], Token::Integer(42));
        assert_eq!(toks[1], Token::Float(3.14));
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
        assert_eq!(toks[1], Token::Op("<=".into()));
        assert_eq!(toks[3], Token::Op(">=".into()));
        assert_eq!(toks[5], Token::Op("!=".into()));
        assert_eq!(toks[7], Token::Op("<>".into()));
        assert_eq!(toks[9], Token::Op("==".into()));
        assert_eq!(toks[11], Token::Op("||".into()));
        assert_eq!(toks[13], Token::Op("<<".into()));
        assert_eq!(toks[15], Token::Op(">>".into()));
    }

    #[test]
    fn comments() {
        let toks = lex("SELECT 1 -- comment\n+ /* block */ 2");
        assert_eq!(toks[0], Token::Keyword("SELECT".into()));
        assert_eq!(toks[1], Token::Integer(1));
        assert_eq!(toks[2], Token::Op("+".into()));
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
