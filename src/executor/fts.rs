//! PostgreSQL-style full-text search: `tsvector` + `tsquery` over TEXT.
//!
//! Text search in PostgreSQL models a document as a **tsvector** (lexemes
//! with positions) and a query as a **tsquery** (a boolean expression over
//! lexemes with phrase `<->` and prefix `:*` matching). Both types have a
//! canonical text form:
//!
//! * tsvector: `'cat':1,3 'dog':2` (lexemes sorted; positions ascending)
//! * tsquery:  `'cat' & 'dog' | !'pig'` / `'cat' <-> 'dog'` / `'run':*`
//!
//! That text form is how we store them in this engine's TEXT storage class
//! — the same layering SQLite's FTS5 uses (inverted structures live above
//! the row store, not inside it). The recommended indexing pattern is a
//! generated column plus an index:
//!
//! ```sql
//! CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT,
//!                   tsv TEXT GENERATED ALWAYS AS (to_tsvector('english', body)));
//! CREATE INDEX docs_tsv ON docs(tsv);
//! SELECT * FROM docs WHERE tsv @@ to_tsquery('english', 'postgres & index');
//! ```
//!
//! The dictionary (stop words + stemming) is applied at **query-formation
//! time** (`to_tsquery` family) and **indexing time** (`to_tsvector`) —
//! never when comparing two already-typed values (`@@`, `ts_rank`), which
//! parse their operands as stored. Results are therefore self-consistent
//! even where our simplified stemmer differs from PostgreSQL's full
//! Snowball english dictionary (cross-database stem equality is not
//! promised).
//!
//! Documented divergences from PostgreSQL:
//! * `ts_rank` uses a simplified frequency model (see [`rank_impl`]);
//!   `ts_rank_cd` is accepted as an alias (cover-density approximated by
//!   the same model). The optional `normalization` integer is accepted
//!   but ignored — length normalization is always applied.
//! * tsvector positions carry no A–D weights, so a weights array only
//!   changes the D weight in practice.
//! * `ts_headline` wraps every match in `<b>...</b>` with no windowing,
//!   ellipsis, or `StartSel/StopSel` options.
//! * A stop word inside a phrase (`'over <-> lazy'`) errors instead of
//!   silently breaking the position chain.
//! * `||` tsvector concatenation deduplicates positions per lexeme.

use crate::error::{Error, Result};
use crate::types::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Maximum position PostgreSQL tracks (positions beyond this are clamped
/// to 16383).
const MAX_POSITION: u16 = 16383;

// ============================================================================
// Text-search configuration
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TsConfig {
    /// Lowercase + stop words + simplified suffix stemmer.
    English,
    /// Lowercase only: no stop words, no stemming.
    Simple,
}

impl TsConfig {
    fn resolve(name: &str) -> Result<TsConfig> {
        // Accept bare names and pg_catalog-qualified ones.
        let n = name
            .rsplit('.')
            .next()
            .unwrap_or(name)
            .trim()
            .to_ascii_lowercase();
        match n.as_str() {
            "english" => Ok(TsConfig::English),
            "simple" => Ok(TsConfig::Simple),
            _ => Err(Error::runtime(format!(
                "text search configuration \"{name}\" is not recognized (supported: english, simple)"
            ))),
        }
    }

    /// Apply the config's dictionary to one raw token: stop words return
    /// None, otherwise the (possibly stemmed) lexeme.
    fn lexeme(&self, word: &str) -> Option<String> {
        match self {
            TsConfig::Simple => Some(word.to_string()),
            TsConfig::English => {
                if is_stop_word(word) {
                    None
                } else {
                    Some(stem(word))
                }
            }
        }
    }
}

// ============================================================================
// Tokenizer
// ============================================================================

/// One raw document token (before the dictionary).
struct RawToken {
    word: String,
    pos: u16,
}

/// Split text into word tokens. A token is a run of alphanumeric (Unicode)
/// characters; in-word apostrophes are dropped (`cat's` → `cats`,
/// `don't` → `dont`) after the possessive `'s` is removed. Positions are
/// 1-based ordinals over ALL tokens (stop words count toward positions,
/// matching PostgreSQL — `'over the lazy'` puts `lazy` at position 3).
fn tokenize(text: &str) -> Vec<RawToken> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut pos: u16 = 0;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if !started {
                started = true;
                pos = pos.saturating_add(1);
            }
            cur.push(c);
        } else if c == '\'' && started {
            // Apostrophe inside a word: `cat's` / `don't`. Attach (no
            // position bump) and let normalization below clean it up.
            cur.push(c);
        } else if started {
            out.push(RawToken {
                word: normalize_word(&cur),
                pos: pos.min(MAX_POSITION),
            });
            cur.clear();
            started = false;
        }
    }
    if started {
        out.push(RawToken {
            word: normalize_word(&cur),
            pos: pos.min(MAX_POSITION),
        });
    }
    out
}

/// Apostrophe normalization for one word: strip a trailing possessive
/// `'s`, then drop remaining apostrophes (`don't` → `dont`).
fn normalize_word(w: &str) -> String {
    let lower = w.to_lowercase();
    let trimmed = lower.strip_suffix("'s").unwrap_or(&lower);
    trimmed.chars().filter(|&c| c != '\'').collect()
}

// ============================================================================
// English stop words (Snowball english list; apostrophe-free variants of
// the contracted forms included to match our tokenizer)
// ============================================================================

/// English stop words (Snowball english list; apostrophe-free variants
/// of the contracted forms included to match our tokenizer). SORTED —
/// [`is_stop_word`] binary-searches it.
const STOP_WORDS: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "arent",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "cannot",
    "cant",
    "could",
    "couldnt",
    "did",
    "didnt",
    "do",
    "does",
    "doesnt",
    "doing",
    "dont",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "hadnt",
    "has",
    "hasnt",
    "have",
    "havent",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "isnt",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "mustnt",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "ought",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "shant",
    "she",
    "should",
    "shouldnt",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "wasnt",
    "we",
    "were",
    "werent",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "wont",
    "would",
    "wouldnt",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

fn is_stop_word(w: &str) -> bool {
    STOP_WORDS.binary_search(&w).is_ok()
}

// ============================================================================
// Simplified English suffix stemmer (Porter-lite)
// ============================================================================

/// Conservative suffix stripper applied identically at index and query
/// formation time. Not a full Snowball implementation — the contract is
/// internal consistency (`running` and `runs` both reduce to `run`, so a
/// query for one matches the other) rather than byte-parity with
/// PostgreSQL's stems (e.g. we keep `database` whole where Snowball
/// produces `databas`).
pub fn stem(word: &str) -> String {
    let mut s = word.to_string();

    // --- step 1: plurals (Porter2 step 1a, approximately) ---
    if s.ends_with("sses") {
        // caresses -> caress
        s.truncate(s.len() - 2);
        return finish(s);
    }
    if s.ends_with("ies") && s.len() >= 5 {
        // ponies -> poni, cries -> cri (ties, len 4, falls through to 'tie')
        s.truncate(s.len() - 2);
        return finish(s);
    }
    if s.ends_with("ss") || s.ends_with("us") || s.len() < 4 {
        return finish(s);
    }
    if s.ends_with("es") {
        // -es after sibilants: foxes -> fox, churches -> church (h covers
        // ch/sh), buzzes -> buzz. Otherwise fall through to plain -s
        // (notes -> note, apples -> apple).
        let before = s.as_bytes()[s.len() - 3];
        if matches!(before, b's' | b'x' | b'z' | b'h') {
            s.truncate(s.len() - 2);
            return finish(s);
        }
    }
    if s.ends_with('s') {
        // cats -> cat (guarded: gas/its/was are len < 4 or stopwords)
        s.truncate(s.len() - 1);
        return finish(s);
    }

    // --- step 2: -eed (agreed -> agree, feed len 4 stays) ---
    if s.ends_with("eed") && s.len() >= 6 {
        s.truncate(s.len() - 1);
        return finish(s);
    }

    // --- step 3: -ing / -ed with fixups (running -> run, hoping -> hope) ---
    for suffix in ["ing", "ed"] {
        if s.ends_with(suffix) {
            let base = s[..s.len() - suffix.len()].to_string();
            if base.chars().count() >= 3 {
                return finish(fixup(&base));
            }
            return finish(s);
        }
    }

    // --- step 4: -ly / -ment / -ness ---
    if s.ends_with("ly") && s.len() >= 6 {
        s.truncate(s.len() - 2);
        return finish(s);
    }
    if s.ends_with("ment") && s.len() >= 8 {
        s.truncate(s.len() - 4);
        return finish(s);
    }
    if s.ends_with("ness") && s.len() >= 8 {
        s.truncate(s.len() - 4);
        return finish(s);
    }
    finish(s)
}

/// Final pass applied to every stem: `y` → `i` when preceded by a
/// consonant that is not the word's first letter (lazy → lazi,
/// happy → happi; day → day, cry → cry — matching Porter2 step 1c's
/// spirit with simpler conditions).
fn finish(mut s: String) -> String {
    let b = s.as_bytes();
    if b.len() >= 3 && b[b.len() - 1] == b'y' {
        let before = b[b.len() - 2];
        if !matches!(before, b'a' | b'e' | b'i' | b'o' | b'u' | b'y') {
            s.pop();
            s.push('i');
        }
    }
    s
}

/// Post-strip fixup: collapse doubled final consonants (`running` →
/// `runn` → `run`, `hopped` → `hopp` → `hop`) and restore a silent `e`
/// after CVC bases (`hoping` → `hop` → `hope`, `typed` → `typ` → `type`;
/// y counts as a vowel here, as in Porter).
fn fixup(base: &str) -> String {
    let b = base.as_bytes();
    if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] {
        let last = b[b.len() - 1];
        // Porter keeps doubled l/s/z (controlled -> control happens in a
        // later step; we keep it simple and do not strip those).
        if !matches!(last, b'l' | b's' | b'z') {
            return base[..base.len() - 1].to_string();
        }
    }
    if b.len() >= 3 {
        let (c1, v, c2) = (b[b.len() - 3], b[b.len() - 2], b[b.len() - 1]);
        let is_cons =
            |c: u8| c.is_ascii_lowercase() && !matches!(c, b'a' | b'e' | b'i' | b'o' | b'u' | b'y');
        let is_vowel = |c: u8| matches!(c, b'a' | b'e' | b'i' | b'o' | b'u' | b'y');
        if is_cons(c1) && is_vowel(v) && is_cons(c2) && !matches!(c2, b'w' | b'x' | b'y') {
            return format!("{base}e");
        }
    }
    base.to_string()
}

// ============================================================================
// tsvector
// ============================================================================

/// A parsed tsvector: lexemes with ascending, deduplicated positions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TsVector {
    pub lexemes: BTreeMap<String, Vec<u16>>,
}

impl TsVector {
    /// Total number of positions across all lexemes (document length).
    fn n_positions(&self) -> usize {
        self.lexemes.values().map(|p| p.len()).sum()
    }

    fn has(&self, word: &str) -> bool {
        self.lexemes.contains_key(word)
    }

    fn positions(&self, word: &str) -> Option<&Vec<u16>> {
        self.lexemes.get(word)
    }

    fn has_prefix(&self, prefix: &str) -> bool {
        self.lexemes.keys().any(|k| k.starts_with(prefix))
    }

    fn has_at(&self, word: &str, pos: u16) -> bool {
        self.positions(word).is_some_and(|p| p.contains(&pos))
    }

    fn has_prefix_at(&self, prefix: &str, pos: u16) -> bool {
        self.lexemes
            .iter()
            .any(|(k, p)| k.starts_with(prefix) && p.contains(&pos))
    }

    /// Canonical text form: `'lex':1,3 'lex2':2` (lexemes sorted).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (lex, positions) in &self.lexemes {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push('\'');
            out.push_str(&lex.replace('\'', "\\'"));
            out.push('\'');
            if !positions.is_empty() {
                out.push(':');
                let joined: Vec<String> = positions.iter().map(|p| p.to_string()).collect();
                out.push_str(&joined.join(","));
            }
        }
        out
    }
}

/// Build a tsvector from raw document text under a config.
pub fn to_tsvector(cfg: TsConfig, text: &str) -> TsVector {
    let mut map: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    for tok in tokenize(text) {
        if tok.word.is_empty() {
            continue;
        }
        if let Some(lex) = cfg.lexeme(&tok.word) {
            if !lex.is_empty() {
                map.entry(lex).or_default().insert(tok.pos);
            }
        }
    }
    TsVector {
        lexemes: map
            .into_iter()
            .map(|(k, set)| (k, set.into_iter().collect()))
            .collect(),
    }
}

/// Parse a tsvector from its canonical text form: `'lex':1,2 'lex2'`.
/// Accepts lexemes without positions; errors on anything else (matching
/// PostgreSQL's cast strictness).
pub fn parse_tsvector(s: &str) -> Result<TsVector> {
    let mut map: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    let b = s.as_bytes();
    let mut i = 0usize;
    loop {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        if b[i] != b'\'' {
            return Err(Error::runtime("syntax error in tsvector"));
        }
        i += 1;
        let mut lex = String::new();
        let mut closed = false;
        while i < b.len() {
            if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\'' {
                lex.push('\'');
                i += 2;
            } else if b[i] == b'\'' {
                i += 1;
                closed = true;
                break;
            } else {
                lex.push(b[i] as char);
                i += 1;
            }
        }
        if !closed || lex.is_empty() {
            return Err(Error::runtime("syntax error in tsvector"));
        }
        // optional :positions
        if i < b.len() && b[i] == b':' {
            i += 1;
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b',') {
                i += 1;
            }
            if i == start {
                return Err(Error::runtime("syntax error in tsvector"));
            }
            for part in s[start..i].split(',') {
                let p: u64 = part
                    .parse()
                    .map_err(|_| Error::runtime("syntax error in tsvector"))?;
                if p == 0 || p > MAX_POSITION as u64 {
                    return Err(Error::runtime(
                        "position value must be greater than 0 and no larger than 16383",
                    ));
                }
                map.entry(lex.clone())
                    .or_default()
                    .insert(p.min(MAX_POSITION as u64) as u16);
            }
        } else {
            map.entry(lex).or_default();
        }
        // next must be whitespace or EOF
        if i < b.len() && !(b[i] as char).is_whitespace() {
            return Err(Error::runtime("syntax error in tsvector"));
        }
    }
    Ok(TsVector {
        lexemes: map
            .into_iter()
            .map(|(k, set)| (k, set.into_iter().collect()))
            .collect(),
    })
}

/// `strip()` — drop positions, keep lexemes.
fn strip(v: &TsVector) -> TsVector {
    TsVector {
        lexemes: v.lexemes.keys().map(|k| (k.clone(), Vec::new())).collect(),
    }
}

/// Concatenate two tsvectors (Postgres `||`): merge lexeme sets and
/// position lists. Positions may collide between the operands (same as
/// Postgres, which does not renumber); duplicate positions are
/// deduplicated per lexeme.
fn tsvector_concat(a: &TsVector, b: &TsVector) -> TsVector {
    let mut out = a.clone();
    for (lex, positions) in &b.lexemes {
        let entry = out.lexemes.entry(lex.clone()).or_default();
        for &p in positions {
            if !entry.contains(&p) {
                entry.push(p);
            }
        }
        entry.sort_unstable();
    }
    out
}

// ============================================================================
// tsquery
// ============================================================================

/// A parsed tsquery.
#[derive(Clone, Debug, PartialEq)]
pub enum TsQuery {
    /// A lexeme; `prefix` = the `:*` suffix match.
    Lex {
        word: String,
        prefix: bool,
    },
    And(Box<TsQuery>, Box<TsQuery>),
    Or(Box<TsQuery>, Box<TsQuery>),
    Not(Box<TsQuery>),
    /// Followed-by chain: element i must match at position p+i.
    Phrase(Vec<TsQuery>),
}

impl TsQuery {
    /// Collect lexemes reachable through POSITIVE branches only (NOT
    /// subtrees skipped) — the terms that can contribute to a rank score.
    fn positive_lexemes(&self, out: &mut Vec<(String, bool)>) {
        match self {
            TsQuery::Not(_) => {}
            TsQuery::Lex { word, prefix } => out.push((word.clone(), *prefix)),
            TsQuery::And(a, b) | TsQuery::Or(a, b) => {
                a.positive_lexemes(out);
                b.positive_lexemes(out);
            }
            TsQuery::Phrase(elems) => {
                for e in elems {
                    e.positive_lexemes(out);
                }
            }
        }
    }

    /// Number of nodes (Postgres `numnode`).
    fn numnode(&self) -> i64 {
        1 + match self {
            TsQuery::Lex { .. } => 0,
            TsQuery::And(a, b) | TsQuery::Or(a, b) => a.numnode() + b.numnode(),
            TsQuery::Not(n) => n.numnode(),
            TsQuery::Phrase(elems) => elems.iter().map(|e| e.numnode()).sum::<i64>(),
        }
    }

    /// Canonical text form with minimal parentheses.
    pub fn render(&self) -> String {
        match self {
            TsQuery::Lex { word, prefix } => {
                let mut s = String::from("'");
                s.push_str(&word.replace('\'', "\\'"));
                s.push('\'');
                if *prefix {
                    s.push_str(":*");
                }
                s
            }
            TsQuery::Not(n) => {
                let inner = n.render();
                if matches!(**n, TsQuery::Lex { .. } | TsQuery::Not(_)) {
                    format!("!{inner}")
                } else {
                    format!("!({inner})")
                }
            }
            TsQuery::Phrase(elems) => {
                let parts: Vec<String> = elems
                    .iter()
                    .map(|e| {
                        let r = e.render();
                        if matches!(e, TsQuery::Lex { .. }) {
                            r
                        } else {
                            format!("({r})")
                        }
                    })
                    .collect();
                parts.join(" <-> ")
            }
            TsQuery::And(a, b) => {
                // & binds tighter than | : parenthesize Or children.
                let l = a.render();
                let r = b.render();
                let l = if matches!(**a, TsQuery::Or(..)) {
                    format!("({l})")
                } else {
                    l
                };
                let r = if matches!(**b, TsQuery::Or(..)) {
                    format!("({r})")
                } else {
                    r
                };
                format!("{l} & {r}")
            }
            TsQuery::Or(a, b) => {
                let l = a.render();
                let r = b.render();
                format!("{l} | {r}")
            }
        }
    }
}

/// Parse a tsquery from text: `cat & dog`, `cat | !pig`, `cat <-> dog`,
/// `run:*`, quoted lexemes, parentheses. The dictionary is applied ONLY
/// when `cfg` is English (call from `to_tsquery`); when comparing stored
/// values (`@@`, `ts_rank`, `numnode`) callers pass [`TsConfig::Simple`]
/// so already-stemmed lexemes are not re-stemmed.
pub fn parse_tsquery(cfg: TsConfig, s: &str) -> Result<TsQuery> {
    let tokens = lex_tsquery(s)?;
    let mut p = TsQueryParser { tokens, pos: 0 };
    let q = p.parse_or()?;
    if p.pos != p.tokens.len() {
        return Err(Error::runtime("syntax error in tsquery"));
    }
    if let TsConfig::English = cfg {
        return simplify(q, cfg);
    }
    Ok(q)
}

#[derive(Clone, Debug, PartialEq)]
enum TsTok {
    Word(String),
    Amp,
    Pipe,
    Bang,
    FollowedBy,
    LParen,
    RParen,
    PrefixMark, // ':*'
}

fn lex_tsquery(s: &str) -> Result<Vec<TsTok>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i] as char;
        if c.is_whitespace() {
            i += 1;
        } else if c == '&' {
            out.push(TsTok::Amp);
            i += 1;
        } else if c == '|' {
            out.push(TsTok::Pipe);
            i += 1;
        } else if c == '!' {
            out.push(TsTok::Bang);
            i += 1;
        } else if c == '(' {
            out.push(TsTok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(TsTok::RParen);
            i += 1;
        } else if c == '\'' {
            i += 1;
            let mut w = String::new();
            let mut closed = false;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\'' {
                    w.push('\'');
                    i += 2;
                } else if b[i] == b'\'' {
                    i += 1;
                    closed = true;
                    break;
                } else {
                    w.push(b[i] as char);
                    i += 1;
                }
            }
            if !closed {
                return Err(Error::runtime("syntax error in tsquery"));
            }
            out.push(TsTok::Word(w));
        } else if c.is_alphanumeric() || c == '_' || c == '-' {
            let start = i;
            while i < b.len() && ((b[i] as char).is_alphanumeric() || matches!(b[i], b'_' | b'-')) {
                i += 1;
            }
            out.push(TsTok::Word(s[start..i].to_string()));
        } else if c == '<' && s[i..].starts_with("<->") {
            out.push(TsTok::FollowedBy);
            i += 3;
        } else if c == ':' && i + 1 < b.len() && b[i + 1] == b'*' {
            out.push(TsTok::PrefixMark);
            i += 2;
        } else {
            return Err(Error::runtime("syntax error in tsquery"));
        }
    }
    Ok(out)
}

struct TsQueryParser {
    tokens: Vec<TsTok>,
    pos: usize,
}

impl TsQueryParser {
    fn peek(&self) -> Option<&TsTok> {
        self.tokens.get(self.pos)
    }

    /// parse_or := parse_and ( '|' parse_and )*
    fn parse_or(&mut self) -> Result<TsQuery> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&TsTok::Pipe) {
            self.pos += 1;
            let right = self.parse_and()?;
            left = TsQuery::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// parse_and := parse_phrase ( '&' parse_phrase )*
    fn parse_and(&mut self) -> Result<TsQuery> {
        let mut left = self.parse_phrase()?;
        while self.peek() == Some(&TsTok::Amp) {
            self.pos += 1;
            let right = self.parse_phrase()?;
            left = TsQuery::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// parse_phrase := parse_not ( '<->' parse_not )* — left-associative
    /// flattening: `a <-> b <-> c` is ONE three-element chain.
    fn parse_phrase(&mut self) -> Result<TsQuery> {
        let first = self.parse_not()?;
        if self.peek() != Some(&TsTok::FollowedBy) {
            return Ok(first);
        }
        let mut elems = vec![first];
        while self.peek() == Some(&TsTok::FollowedBy) {
            self.pos += 1;
            let next = self.parse_not()?;
            match next {
                TsQuery::Phrase(mut inner) => elems.append(&mut inner),
                other => elems.push(other),
            }
        }
        Ok(TsQuery::Phrase(elems))
    }

    /// parse_not := '!' parse_not | parse_atom
    fn parse_not(&mut self) -> Result<TsQuery> {
        if self.peek() == Some(&TsTok::Bang) {
            self.pos += 1;
            let inner = self.parse_not()?;
            return Ok(TsQuery::Not(Box::new(inner)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<TsQuery> {
        match self.peek().cloned() {
            Some(TsTok::LParen) => {
                self.pos += 1;
                let q = self.parse_or()?;
                if self.peek() != Some(&TsTok::RParen) {
                    return Err(Error::runtime("syntax error in tsquery"));
                }
                self.pos += 1;
                Ok(q)
            }
            Some(TsTok::Word(w)) => {
                self.pos += 1;
                let prefix = if self.peek() == Some(&TsTok::PrefixMark) {
                    self.pos += 1;
                    true
                } else {
                    false
                };
                Ok(TsQuery::Lex {
                    word: normalize_word(&w),
                    prefix,
                })
            }
            _ => Err(Error::runtime("syntax error in tsquery")),
        }
    }
}

/// Apply the dictionary (stop words + stemming) to every lexeme, then
/// restructure the tree: `x & <dropped>` → `x`; a fully-dropped query
/// errors like PostgreSQL ("query contains only stop words").
fn simplify(q: TsQuery, cfg: TsConfig) -> Result<TsQuery> {
    match q {
        TsQuery::Lex { word, prefix } => match cfg.lexeme(&word) {
            Some(lex) => Ok(TsQuery::Lex { word: lex, prefix }),
            None => Err(Error::runtime(format!(
                "stop word \"{word}\" ignored in query"
            ))),
        },
        TsQuery::Not(n) => {
            let word = lexeme_word(&n);
            match simplify_drop(*n, cfg)? {
                Some(inner) => Ok(TsQuery::Not(Box::new(inner))),
                None => Err(Error::runtime(format!(
                    "stop word \"{word}\" ignored in query"
                ))),
            }
        }
        TsQuery::And(a, b) => match (simplify_drop(*a, cfg)?, simplify_drop(*b, cfg)?) {
            (Some(x), Some(y)) => Ok(TsQuery::And(Box::new(x), Box::new(y))),
            (Some(x), None) | (None, Some(x)) => Ok(x),
            (None, None) => Err(Error::runtime("query contains only stop words")),
        },
        TsQuery::Or(a, b) => match (simplify_drop(*a, cfg)?, simplify_drop(*b, cfg)?) {
            (Some(x), Some(y)) => Ok(TsQuery::Or(Box::new(x), Box::new(y))),
            (Some(x), None) | (None, Some(x)) => Ok(x),
            (None, None) => Err(Error::runtime("query contains only stop words")),
        },
        TsQuery::Phrase(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                // NOT inside a phrase has no position semantics — reject.
                if matches!(e, TsQuery::Not(_)) {
                    return Err(Error::runtime(
                        "NOT (!) is not allowed inside a phrase (<->)",
                    ));
                }
                // A dropped stop word would break the position chain.
                let se = simplify_drop(e, cfg)?.ok_or_else(|| {
                    Error::runtime(
                        "stop word inside a phrase cannot be dropped without breaking positions",
                    )
                })?;
                out.push(se);
            }
            Ok(TsQuery::Phrase(out))
        }
    }
}

/// Like [`simplify`] but a dropped stop word yields `None` instead of
/// erroring (the parent decides how to restructure).
fn simplify_drop(q: TsQuery, cfg: TsConfig) -> Result<Option<TsQuery>> {
    match q {
        TsQuery::Lex { word, prefix } => Ok(cfg
            .lexeme(&word)
            .map(|lex| TsQuery::Lex { word: lex, prefix })),
        TsQuery::Not(n) => Ok(Some(TsQuery::Not(Box::new(simplify(*n, cfg)?)))),
        TsQuery::And(a, b) => match (simplify_drop(*a, cfg)?, simplify_drop(*b, cfg)?) {
            (Some(x), Some(y)) => Ok(Some(TsQuery::And(Box::new(x), Box::new(y)))),
            (Some(x), None) | (None, Some(x)) => Ok(Some(x)),
            (None, None) => Ok(None),
        },
        TsQuery::Or(a, b) => match (simplify_drop(*a, cfg)?, simplify_drop(*b, cfg)?) {
            (Some(x), Some(y)) => Ok(Some(TsQuery::Or(Box::new(x), Box::new(y)))),
            (Some(x), None) | (None, Some(x)) => Ok(Some(x)),
            (None, None) => Ok(None),
        },
        TsQuery::Phrase(elems) => {
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                if matches!(e, TsQuery::Not(_)) {
                    return Err(Error::runtime(
                        "NOT (!) is not allowed inside a phrase (<->)",
                    ));
                }
                match simplify_drop(e, cfg)? {
                    Some(se) => out.push(se),
                    None => return Err(Error::runtime(
                        "stop word inside a phrase cannot be dropped without breaking positions",
                    )),
                }
            }
            if out.is_empty() {
                return Ok(None);
            }
            Ok(Some(TsQuery::Phrase(out)))
        }
    }
}

/// The surface word of a (possibly nested) query node — for error text.
fn lexeme_word(q: &TsQuery) -> String {
    match q {
        TsQuery::Lex { word, .. } => word.clone(),
        TsQuery::Not(n) => lexeme_word(n),
        _ => String::new(),
    }
}

/// plainto_tsquery: every non-stop lexeme AND-ed together.
pub fn plainto_tsquery(cfg: TsConfig, text: &str) -> Result<TsQuery> {
    let mut terms: Vec<TsQuery> = Vec::new();
    for tok in tokenize(text) {
        if tok.word.is_empty() {
            continue;
        }
        if let Some(lex) = cfg.lexeme(&tok.word) {
            if !lex.is_empty() {
                terms.push(TsQuery::Lex {
                    word: lex,
                    prefix: false,
                });
            }
        }
    }
    reduce_and(terms)
}

/// phraseto_tsquery: every non-stop lexeme chained with `<->`.
pub fn phraseto_tsquery(cfg: TsConfig, text: &str) -> Result<TsQuery> {
    let mut terms: Vec<TsQuery> = Vec::new();
    for tok in tokenize(text) {
        if tok.word.is_empty() {
            continue;
        }
        if let Some(lex) = cfg.lexeme(&tok.word) {
            if !lex.is_empty() {
                terms.push(TsQuery::Lex {
                    word: lex,
                    prefix: false,
                });
            }
        }
    }
    if terms.is_empty() {
        return Err(Error::runtime("query contains only stop words"));
    }
    Ok(TsQuery::Phrase(terms))
}

/// websearch_to_tsquery: `"quoted phrases"` → `<->` chains, `OR` → `|`,
/// `-term` → NOT, bare `+`/`AND` → the default AND. A trailing `OR` with
/// no following term is dropped (PostgreSQL behavior).
pub fn websearch_to_tsquery(cfg: TsConfig, text: &str) -> Result<TsQuery> {
    #[derive(Debug)]
    enum Item {
        Term(TsQuery),
        OrMark,
    }
    let mut items: Vec<Item> = Vec::new();
    let mut in_quotes = false;
    let mut phrase_buf: Vec<TsQuery> = Vec::new();

    fn flush_phrase(buf: &mut Vec<TsQuery>, items: &mut Vec<Item>, negated: bool) {
        if buf.is_empty() {
            return;
        }
        let phrase = TsQuery::Phrase(std::mem::take(buf));
        items.push(Item::Term(if negated {
            TsQuery::Not(Box::new(phrase))
        } else {
            phrase
        }));
    }

    for raw in text.split_whitespace() {
        let mut tok: &str = raw;
        let mut negated = false;
        if !in_quotes {
            if let Some(rest) = tok.strip_prefix('-') {
                if !rest.is_empty() {
                    negated = true;
                    tok = rest;
                }
            }
        }
        if tok.contains('"') {
            let stripped: String = tok.chars().filter(|&c| c != '"').collect();
            if in_quotes {
                in_quotes = false;
                push_lexemes(&stripped, cfg, &mut phrase_buf);
                flush_phrase(&mut phrase_buf, &mut items, negated);
            } else {
                in_quotes = true;
                push_lexemes(&stripped, cfg, &mut phrase_buf);
            }
            continue;
        }
        if in_quotes {
            push_lexemes(tok, cfg, &mut phrase_buf);
            continue;
        }
        if tok.eq_ignore_ascii_case("OR") {
            items.push(Item::OrMark);
            continue;
        }
        if tok.eq_ignore_ascii_case("AND") || tok == "+" {
            continue; // explicit AND is the default
        }
        let mut one = Vec::new();
        push_lexemes(tok, cfg, &mut one);
        for q in one {
            items.push(Item::Term(if negated {
                TsQuery::Not(Box::new(q))
            } else {
                q
            }));
        }
    }
    // unterminated quote: flush as a phrase
    flush_phrase(&mut phrase_buf, &mut items, false);

    // Fold items left-to-right: Term × Term → And, Term OrMark Term → Or.
    let mut acc: Option<TsQuery> = None;
    let mut pending_or = false;
    for item in items {
        match item {
            Item::OrMark => pending_or = true,
            Item::Term(t) => {
                acc = Some(match acc {
                    None => t,
                    Some(prev) => {
                        if pending_or {
                            TsQuery::Or(Box::new(prev), Box::new(t))
                        } else {
                            TsQuery::And(Box::new(prev), Box::new(t))
                        }
                    }
                });
                pending_or = false;
            }
        }
    }
    match acc {
        Some(q) => Ok(q),
        None => Err(Error::runtime("query contains only stop words")),
    }
}

fn reduce_and(terms: Vec<TsQuery>) -> Result<TsQuery> {
    if terms.is_empty() {
        return Err(Error::runtime("query contains only stop words"));
    }
    Ok(terms
        .into_iter()
        .reduce(|a, b| TsQuery::And(Box::new(a), Box::new(b)))
        .expect("non-empty"))
}

fn push_lexemes(tok: &str, cfg: TsConfig, out: &mut Vec<TsQuery>) {
    for t in tokenize(tok) {
        if t.word.is_empty() {
            continue;
        }
        if let Some(lex) = cfg.lexeme(&t.word) {
            if !lex.is_empty() {
                out.push(TsQuery::Lex {
                    word: lex,
                    prefix: false,
                });
            }
        }
    }
}

// ============================================================================
// Matching
// ============================================================================

/// Evaluate `tsvector @@ tsquery` (both operands non-NULL).
pub fn ts_match(v: &TsVector, q: &TsQuery) -> bool {
    match q {
        TsQuery::Lex { word, prefix } => {
            if *prefix {
                v.has_prefix(word)
            } else {
                v.has(word)
            }
        }
        TsQuery::And(a, b) => ts_match(v, a) && ts_match(v, b),
        TsQuery::Or(a, b) => ts_match(v, a) || ts_match(v, b),
        TsQuery::Not(n) => !ts_match(v, n),
        TsQuery::Phrase(elems) => {
            // exists p such that elems[i] matches at position p+i
            let max_start = MAX_POSITION.saturating_sub(elems.len() as u16 - 1);
            (1..=max_start).any(|p| {
                elems
                    .iter()
                    .enumerate()
                    .all(|(i, e)| query_matches_at(v, e, p + i as u16))
            })
        }
    }
}

/// Does this (phrase-element) query hold at exactly one position?
/// AND/OR inside a phrase element match per-position; NOT is unreachable
/// (the parser rejects it inside phrases).
fn query_matches_at(v: &TsVector, q: &TsQuery, pos: u16) -> bool {
    match q {
        TsQuery::Lex { word, prefix } => {
            if *prefix {
                v.has_prefix_at(word, pos)
            } else {
                v.has_at(word, pos)
            }
        }
        TsQuery::And(a, b) => query_matches_at(v, a, pos) && query_matches_at(v, b, pos),
        TsQuery::Or(a, b) => query_matches_at(v, a, pos) || query_matches_at(v, b, pos),
        TsQuery::Not(_) => false,
        TsQuery::Phrase(elems) => elems
            .iter()
            .enumerate()
            .all(|(i, e)| query_matches_at(v, e, pos + i as u16)),
    }
}

// ============================================================================
// Ranking
// ============================================================================

/// Simplified PostgreSQL ts_rank: every positively-matched lexeme
/// contributes `w_D * (1 + ln(f))` where f is the lexeme's position
/// count and w_D the D weight (all positions are weight-D here — no
/// A–D markup); the sum is divided by `(1 + D * N)` with N the
/// document's total position count and D the dampening constant
/// (default 0.1) — PostgreSQL's length normalization.
fn rank_impl(v: &TsVector, q: &TsQuery, weights: &[f64; 4], d: f64) -> f64 {
    let mut lexemes = Vec::new();
    q.positive_lexemes(&mut lexemes);
    let mut sum = 0.0;
    for (word, prefix) in &lexemes {
        let matched = if *prefix {
            v.has_prefix(word)
        } else {
            v.has(word)
        };
        if !matched {
            continue;
        }
        let f = v.positions(word).map(|p| p.len()).unwrap_or(1).max(1) as f64;
        let w = weights[0].max(0.0);
        sum += w * (1.0 + f.ln());
    }
    let n = v.n_positions().max(1) as f64;
    sum / (1.0 + d.abs().min(16.0) * n)
}

// ============================================================================
// Headline
// ============================================================================

/// Wrap matched tokens in `<b>...</b>`. Simplified vs PostgreSQL: no
/// windowing, no ellipsis, no StartSel/StopSel options — every match is
/// wrapped in place.
fn ts_headline(cfg: TsConfig, text: &str, q: &TsQuery) -> String {
    let mut terms = Vec::new();
    q.positive_lexemes(&mut terms);
    let mut out = String::new();
    let mut last_end = 0usize;
    for tok in tokenize(text) {
        if tok.word.is_empty() {
            continue;
        }
        let Some(stemmed) = cfg.lexeme(&tok.word) else {
            continue;
        };
        let hit = terms.iter().any(|(word, prefix)| {
            if *prefix {
                stemmed.starts_with(word.as_str())
            } else {
                stemmed == *word
            }
        });
        if hit {
            // Locate this occurrence in the remaining original text;
            // `find` from `last_end` preserves token order.
            if let Some(off) = text[last_end..].find(&tok.word) {
                let abs = last_end + off;
                out.push_str(&text[last_end..abs]);
                out.push_str("<b>");
                out.push_str(&text[abs..abs + tok.word.len()]);
                out.push_str("</b>");
                last_end = abs + tok.word.len();
            }
        }
    }
    out.push_str(&text[last_end..]);
    out
}

// ============================================================================
// SQL function dispatch
// ============================================================================

fn text_arg(args: &[Value], i: usize) -> Result<String> {
    match args.get(i) {
        Some(v) if !v.is_null() => Ok(v.as_text()),
        _ => Err(Error::runtime("text search argument must not be NULL")),
    }
}

/// Config-or-value argument resolution for `f([config,] text)`
/// signatures (PostgreSQL's default config here: english). Returns
/// `Ok(None)` when any argument is NULL — these functions are STRICT in
/// PostgreSQL (NULL text — or a NULL config — yields NULL, never an
/// error).
fn config_and_text(args: &[Value]) -> Result<Option<(TsConfig, String)>> {
    match args.len() {
        1 => {
            if args[0].is_null() {
                return Ok(None);
            }
            Ok(Some((TsConfig::English, text_arg(args, 0)?)))
        }
        2 => {
            if args[0].is_null() || args[1].is_null() {
                return Ok(None);
            }
            let cfg = TsConfig::resolve(&args[0].as_text())?;
            let text = text_arg(args, 1)?;
            Ok(Some((cfg, text)))
        }
        _ => Err(Error::runtime("expected 1 or 2 arguments")),
    }
}

/// Parse `{wD,wC,wB,wA}` (PostgreSQL array literal) into 4 weights
/// (index 0 = D ... 3 = A).
fn parse_weights(v: &Value) -> Result<[f64; 4]> {
    let text = v.as_text();
    let s = text.trim();
    let inner = s
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(|| {
            Error::runtime("weights must be an array literal like '{0.1,0.2,0.4,1.0}'")
        })?;
    let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        return Err(Error::runtime("weights array must have exactly 4 elements"));
    }
    let mut out = [0.1f64; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse()
            .map_err(|_| Error::runtime("invalid weight value"))?;
    }
    Ok(out)
}

fn is_weights_literal(v: &Value) -> bool {
    match v {
        Value::Text(t) => t.as_str().trim_start().starts_with('{'),
        _ => false,
    }
}

/// The `@@` operator: tsvector @@ tsquery (NULL on either side → NULL).
/// Operands are STORED tsvector/tsquery text — parsed with the Simple
/// dictionary so already-stemmed lexemes are never re-stemmed.
pub fn eval_match_op(l: &Value, r: &Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let vec = parse_tsvector(&l.as_text())
        .map_err(|e| Error::runtime(format!("invalid tsvector: {e}")))?;
    let q = parse_tsquery(TsConfig::Simple, &r.as_text())
        .map_err(|e| Error::runtime(format!("invalid tsquery: {e}")))?;
    Ok(Value::Integer(i64::from(ts_match(&vec, &q))))
}

/// Dispatch table for FTS functions. Returns `Ok(None)` when the name is
/// not an FTS function (the caller tries the next family).
pub fn call_fts_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let out = match name {
        "to_tsvector" => {
            let Some((cfg, text)) = config_and_text(args)? else {
                return Ok(Some(Value::Null));
            };
            Value::Text(to_tsvector(cfg, &text).render().into())
        }
        "to_tsquery" => {
            let Some((cfg, text)) = config_and_text(args)? else {
                return Ok(Some(Value::Null));
            };
            Value::Text(parse_tsquery(cfg, &text)?.render().into())
        }
        "plainto_tsquery" => {
            let Some((cfg, text)) = config_and_text(args)? else {
                return Ok(Some(Value::Null));
            };
            Value::Text(plainto_tsquery(cfg, &text)?.render().into())
        }
        "phraseto_tsquery" => {
            let Some((cfg, text)) = config_and_text(args)? else {
                return Ok(Some(Value::Null));
            };
            Value::Text(phraseto_tsquery(cfg, &text)?.render().into())
        }
        "websearch_to_tsquery" => {
            let Some((cfg, text)) = config_and_text(args)? else {
                return Ok(Some(Value::Null));
            };
            Value::Text(websearch_to_tsquery(cfg, &text)?.render().into())
        }
        "ts_match" => {
            // Function form of @@: ts_match(tsvector, tsquery).
            if args.len() != 2 {
                return Err(Error::runtime(
                    "ts_match(tsvector, tsquery) takes 2 arguments",
                ));
            }
            return eval_match_op(&args[0], &args[1]).map(Some);
        }
        "ts_rank" | "ts_rank_cd" => {
            // ts_rank([ weights, ] tsvector, tsquery [, normalization ])
            // — ts_rank_cd accepted as an alias (cover-density approximated
            // by the same frequency model). The optional integer
            // `normalization` is accepted but IGNORED: our rank always
            // applies length normalization (documented divergence).
            let (weights, v, q) = match args.len() {
                2 => ([0.1; 4], &args[0], &args[1]),
                3 if is_weights_literal(&args[0]) => (parse_weights(&args[0])?, &args[1], &args[2]),
                3 => ([0.1; 4], &args[0], &args[1]),
                4 => (parse_weights(&args[0])?, &args[1], &args[2]),
                _ => return Err(Error::runtime(
                    "ts_rank([weights,] tsvector, tsquery [, normalization]) takes 2-4 arguments",
                )),
            };
            if v.is_null() || q.is_null() {
                return Ok(Some(Value::Null));
            }
            let vec = parse_tsvector(&v.as_text())
                .map_err(|e| Error::runtime(format!("invalid tsvector: {e}")))?;
            let query = parse_tsquery(TsConfig::Simple, &q.as_text())
                .map_err(|e| Error::runtime(format!("invalid tsquery: {e}")))?;
            Value::Real(rank_impl(&vec, &query, &weights, 0.1))
        }
        "tsvector_concat" => {
            if args.len() != 2 {
                return Err(Error::runtime("tsvector_concat(a, b) takes 2 arguments"));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let a = parse_tsvector(&args[0].as_text())?;
            let b = parse_tsvector(&args[1].as_text())?;
            Value::Text(tsvector_concat(&a, &b).render().into())
        }
        "strip" => {
            let Some(v) = args.first() else {
                return Ok(Some(Value::Null));
            };
            if v.is_null() {
                return Ok(Some(Value::Null));
            }
            let vec = parse_tsvector(&v.as_text())?;
            Value::Text(strip(&vec).render().into())
        }
        "numnode" => {
            let Some(v) = args.first() else {
                return Ok(Some(Value::Null));
            };
            if v.is_null() {
                return Ok(Some(Value::Null));
            }
            let q = parse_tsquery(TsConfig::Simple, &v.as_text())?;
            Value::Integer(q.numnode())
        }
        "ts_headline" => {
            // STRICT like PostgreSQL: NULL document (or config) → NULL.
            let (cfg, text, qv) = match args.len() {
                2 => {
                    if args[0].is_null() {
                        return Ok(Some(Value::Null));
                    }
                    (TsConfig::English, text_arg(args, 0)?, args[1].clone())
                }
                3 => {
                    if args[0].is_null() || args[1].is_null() {
                        return Ok(Some(Value::Null));
                    }
                    let cfg = TsConfig::resolve(&args[0].as_text())?;
                    (cfg, text_arg(args, 1)?, args[2].clone())
                }
                _ => {
                    return Err(Error::runtime(
                        "ts_headline([config,] text, tsquery) takes 2 or 3 arguments",
                    ))
                }
            };
            if qv.is_null() {
                return Ok(Some(Value::Null));
            }
            let q = parse_tsquery(TsConfig::Simple, &qv.as_text())?;
            Value::Text(ts_headline(cfg, &text, &q).into())
        }
        _ => return Ok(None),
    };
    Ok(Some(out))
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> TsVector {
        to_tsvector(TsConfig::English, text)
    }

    #[test]
    fn tokenizer_positions_count_stopwords() {
        // "the quick" → quick at position 2 (the occupies position 1).
        let t = to_tsvector(TsConfig::English, "the quick brown fox");
        assert_eq!(t.positions("quick").unwrap(), &vec![2]);
        assert_eq!(t.positions("brown").unwrap(), &vec![3]);
        assert_eq!(t.positions("fox").unwrap(), &vec![4]);
        assert!(!t.has("the"));
    }

    #[test]
    fn canonical_rendering() {
        // the@1 quick@2 brown@3 foxes@4 jumped@5 over@6 the@7 lazy@8 dogs@9
        // foxes→fox, jumped→jump, lazy→lazi, dogs→dog; the/over stop.
        let t = v("The quick brown foxes jumped over the lazy dogs");
        let rendered = t.render();
        assert_eq!(
            rendered,
            "'brown':3 'dog':9 'fox':4 'jump':5 'lazi':8 'quick':2"
        );
        // round trip
        let re = parse_tsvector(&rendered).unwrap();
        assert_eq!(re, t);
    }

    #[test]
    fn stemmer_basics() {
        assert_eq!(stem("cats"), "cat");
        assert_eq!(stem("dogs"), "dog");
        assert_eq!(stem("foxes"), "fox");
        assert_eq!(stem("churches"), "church");
        assert_eq!(stem("ponies"), "poni");
        assert_eq!(stem("running"), "run");
        assert_eq!(stem("hopping"), "hop");
        assert_eq!(stem("hoping"), "hope");
        assert_eq!(stem("typed"), "type");
        assert_eq!(stem("agreed"), "agree");
        assert_eq!(stem("quickly"), "quick");
        assert_eq!(stem("lazy"), "lazi");
        assert_eq!(stem("happy"), "happi");
        assert_eq!(stem("day"), "day"); // vowel before y
        assert_eq!(stem("database"), "database"); // no final-e strip (documented divergence)
    }

    #[test]
    fn match_and_or_not() {
        let doc = v("PostgreSQL is a powerful open source database engine");
        let q = |s: &str| parse_tsquery(TsConfig::English, s).unwrap();
        assert!(ts_match(&doc, &q("postgresql")));
        assert!(ts_match(&doc, &q("postgresql & database")));
        assert!(ts_match(&doc, &q("postgresql | mysql")));
        assert!(ts_match(&doc, &q("!mysql")));
        assert!(!ts_match(&doc, &q("postgresql & mysql")));
        assert!(!ts_match(&doc, &q("mysql")));
    }

    #[test]
    fn prefix_matching() {
        let doc = v("running quickly through the forest");
        let q = parse_tsquery(TsConfig::English, "run:* & quick:*").unwrap();
        assert!(ts_match(&doc, &q));
        let q2 = parse_tsquery(TsConfig::English, "foresty:*").unwrap();
        assert!(!ts_match(&doc, &q2));
    }

    #[test]
    fn phrase_matching() {
        // the@1 quick@2 brown@3 fox@4 jump@5 over@6 the@7 lazi@8 dog@9
        let doc = v("The quick brown fox jumps over the lazy dog");
        let q = parse_tsquery(TsConfig::English, "quick <-> brown <-> fox").unwrap();
        assert!(ts_match(&doc, &q));
        // wrong order: quick@2, brown@3 — brown then quick needs 3,4
        let q2 = parse_tsquery(TsConfig::English, "brown <-> quick").unwrap();
        assert!(!ts_match(&doc, &q2));
        // a stop word inside a phrase errors (would break positions)
        assert!(parse_tsquery(TsConfig::English, "over <-> lazi").is_err());
    }

    #[test]
    fn stop_word_query_errors() {
        // a query of ONLY stop words errors like PostgreSQL.
        assert!(parse_tsquery(TsConfig::English, "the & was").is_err());
        // stop words dropped from mixed queries:
        let q = parse_tsquery(TsConfig::English, "cat & the").unwrap();
        assert_eq!(q.render(), "'cat'");
    }

    #[test]
    fn plainto_and_websearch() {
        let q = plainto_tsquery(TsConfig::English, "The fat cats").unwrap();
        assert_eq!(q.render(), "'fat' & 'cat'");
        let q2 = websearch_to_tsquery(TsConfig::English, "fat cats mouse").unwrap();
        assert_eq!(q2.render(), "'fat' & 'cat' & 'mouse'");
        let q3 = websearch_to_tsquery(TsConfig::English, "fat OR cats").unwrap();
        assert_eq!(q3.render(), "'fat' | 'cat'");
        let q4 = websearch_to_tsquery(TsConfig::English, "\"fat cats\" -mouse").unwrap();
        assert_eq!(q4.render(), "'fat' <-> 'cat' & !'mouse'");
        let q5 = websearch_to_tsquery(TsConfig::English, "trailing OR").unwrap();
        assert_eq!(q5.render(), "'trail'");
    }

    #[test]
    fn rank_orders_by_frequency() {
        // Simple config on BOTH sides: the query lexeme 'postgres' must
        // equal the stored lexeme (English would stem it to 'postgre').
        let one = to_tsvector(TsConfig::Simple, "postgres appears once");
        let many = to_tsvector(TsConfig::Simple, "postgres postgres postgres everywhere");
        let q = parse_tsquery(TsConfig::Simple, "'postgres'").unwrap();
        let r1 = rank_impl(&one, &q, &[0.1; 4], 0.1);
        let r2 = rank_impl(&many, &q, &[0.1; 4], 0.1);
        assert!(r2 > r1, "rank(many) {r2} should exceed rank(one) {r1}");
        // English side: query for the STEMMED form 'postgre' ranks the
        // English-indexed document too (index/query consistency).
        let en = to_tsvector(TsConfig::English, "postgres postgres postgres everywhere");
        let q_en = parse_tsquery(TsConfig::English, "postgres").unwrap();
        let r_en = rank_impl(&en, &q_en, &[0.1; 4], 0.1);
        assert!(r_en > 0.0, "english rank {r_en} should be positive");
    }

    #[test]
    fn headline_wraps_matches() {
        let q = parse_tsquery(TsConfig::Simple, "'quick' & 'fox'").unwrap();
        let h = ts_headline(TsConfig::English, "The quick brown fox jumps", &q);
        assert!(h.contains("<b>quick</b>"));
        assert!(h.contains("<b>fox</b>"));
        assert!(!h.contains("<b>The</b>"));
    }

    #[test]
    fn strip_concat_numnode() {
        let t = v("cats and dogs");
        let s = strip(&t);
        assert_eq!(s.render(), "'cat' 'dog'");
        let a = v("red apples");
        let b = v("green apples");
        let c = tsvector_concat(&a, &b);
        assert_eq!(c.positions("apple").unwrap(), &vec![2]); // dedup
        assert_eq!(c.render(), "'apple':2 'green':1 'red':1");
        let q = parse_tsquery(TsConfig::Simple, "a & b | !c").unwrap();
        assert_eq!(q.numnode(), 6);
        assert_eq!(q.render(), "'a' & 'b' | !'c'");
    }

    #[test]
    fn parse_tsvector_rejects_garbage() {
        assert!(parse_tsvector("cat dog").is_err());
        assert!(parse_tsvector("'cat':0").is_err());
        assert!(parse_tsvector("'cat':99999").is_err());
        assert!(parse_tsvector("").is_ok());
    }

    #[test]
    fn simple_config_keeps_stopwords() {
        let t = to_tsvector(TsConfig::Simple, "The the THE");
        assert_eq!(t.positions("the").unwrap(), &vec![1, 2, 3]);
    }

    #[test]
    fn match_op_parses_stored_forms() {
        // @@ parses operands as STORED values: already-stemmed lexemes
        // must not be re-stemmed ('running' stays 'running', so it only
        // matches a document whose lexeme is literally 'running').
        let doc = to_tsvector(TsConfig::Simple, "running fast");
        let q_stored = "'running'";
        assert_eq!(
            eval_match_op(
                &Value::Text(doc.render().into()),
                &Value::Text(q_stored.into())
            )
            .unwrap(),
            Value::Integer(1)
        );
        let q_stemmed = "'run'"; // stored 'run' does NOT match 'running'
        assert_eq!(
            eval_match_op(
                &Value::Text(doc.render().into()),
                &Value::Text(q_stemmed.into())
            )
            .unwrap(),
            Value::Integer(0)
        );
        // NULL propagation
        assert_eq!(
            eval_match_op(&Value::Null, &Value::Text(q_stored.into())).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn ts_rank_shapes() {
        let doc = Value::Text(v("postgres index postgres").render().into());
        let q = Value::Text("'postgres'".into());
        // 2-arg
        assert!(call_fts_function("ts_rank", &[doc.clone(), q.clone()])
            .unwrap()
            .is_some_and(|r| matches!(r, Value::Real(_))));
        // 3-arg with weights first
        let w = Value::Text("{0.1,0.2,0.4,1.0}".into());
        assert!(call_fts_function("ts_rank", &[w, doc.clone(), q.clone()])
            .unwrap()
            .is_some_and(|r| matches!(r, Value::Real(_))));
        // 3-arg with normalization int
        let n = Value::Integer(2);
        assert!(call_fts_function("ts_rank", &[doc.clone(), q.clone(), n])
            .unwrap()
            .is_some_and(|r| matches!(r, Value::Real(_))));
        // bad weights
        assert!(call_fts_function("ts_rank", &[Value::Text("{1,2}".into()), doc, q]).is_err());
    }

    #[test]
    fn stop_word_list_is_sorted() {
        // is_stop_word binary-searches STOP_WORDS — keep it sorted.
        assert!(STOP_WORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(is_stop_word("are"));
        assert!(is_stop_word("cannot"));
        assert!(!is_stop_word("cat"));
    }

    #[test]
    fn fts_dispatch_unknown_name() {
        assert!(call_fts_function("nope", &[]).unwrap().is_none());
    }
}
