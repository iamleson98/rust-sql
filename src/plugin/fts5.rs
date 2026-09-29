//! FTS5 — SQLite's modern full-text search module, implemented on the
//! engine's vtab protocol.
//!
//! Architecture: each FTS5 table keeps an in-memory inverted index
//! (term → rowid → per-column positions) built from its documents; the
//! ENGINE persists the documents in the `t_content` shadow table
//! (created with the vtab, transactional through the ordinary pager,
//! replayed into the module at reopen via `reindex`). This mirrors
//! SQLite's fts5 layering — the inverted structure lives above the row
//! store — with the shadow playing `t_content`'s role.
//!
//! Query grammar (SQLite's fts5_expr): barewords and "quoted phrases"
//! (adjacent terms form phrases), implicit AND, `OR`, `NOT`, `NEAR(a b, N)`
//! (column-scoped), column filters `col:term` / `{c1 c2}:term`, prefix
//! `term*`, first-token anchor `^term`, parentheses.
//!
//! Aux surface: the hidden `rank` column (−bm25), `bm25(t, ...)`,
//! `highlight(t, col, pre, post)`, `snippet(t, col, pre, post, ell, n)`.
//!
//! Tokenizers: `unicode61` (default; case-fold + diacritic removal),
//! `ascii`, `porter` (stemmer over unicode61), `trigram` (substring /
//! LIKE-style matching).
//!
//! Table options: `tokenize=`, `prefix=`, `content=` (external-content
//! tables read their values from the content table; `content=''` is
//! contentless — reads return NULL), `content_rowid=`, `columnsize=`,
//! `detail=`, and per-column `UNINDEXED`.

use crate::error::{Error, Result};
use crate::plugin::vtab::{
    IndexInfo, ModuleCaps, ShadowTable, UpdateOp, VirtualTable, VirtualTableCursor,
    VirtualTableModule, VtabConstraint, VtabConstraintOp,
};
use crate::types::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

// ============================================================================
// Configuration
// ============================================================================

/// One declared column.
#[derive(Clone, Debug)]
struct FtsColumn {
    name: String,
    /// UNINDEXED: stored, never tokenized.
    unindexed: bool,
}

/// Tokenizer selection.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tokenizer {
    /// unicode61: alphanumeric runs, case-folded, diacritics removed
    /// (the default).
    Unicode61 {
        remove_diacritics: u8,
        tokenchars: Vec<char>,
        separators: Vec<char>,
    },
    /// ascii: 7-bit tokens only (bytes >= 128 are separators).
    Ascii,
    /// porter: unicode61 followed by the Porter stemmer.
    Porter(Box<Tokenizer>),
    /// trigram: every 3-character window is a token (substring search).
    Trigram,
}

impl Tokenizer {
    fn parse(spec: &str) -> Result<Tokenizer> {
        let mut it = spec.split_whitespace();
        let name = it.next().unwrap_or("unicode61").to_ascii_lowercase();
        let mut remove_diacritics: u8 = 1;
        let mut tokenchars: Vec<char> = Vec::new();
        let mut separators: Vec<char> = Vec::new();
        for opt in it {
            let (k, v) = match opt.split_once('=') {
                Some((k, v)) => (
                    k.to_ascii_lowercase(),
                    v.trim_matches(|c| c == '\'' || c == '"'),
                ),
                None => (opt.to_ascii_lowercase(), ""),
            };
            match k.as_str() {
                "remove_diacritics" => {
                    remove_diacritics = v.parse::<u8>().map_err(|_| {
                        Error::semantic(format!("malformed tokenize option: {opt}"))
                    })?;
                }
                "tokenchars" => tokenchars.extend(v.chars()),
                "separators" => separators.extend(v.chars()),
                _ => return Err(Error::semantic(format!("unknown tokenizer option: {k}"))),
            }
        }
        match name.as_str() {
            "unicode61" => Ok(Tokenizer::Unicode61 {
                remove_diacritics,
                tokenchars,
                separators,
            }),
            "ascii" => Ok(Tokenizer::Ascii),
            "porter" => Ok(Tokenizer::Porter(Box::new(Tokenizer::Unicode61 {
                remove_diacritics,
                tokenchars,
                separators,
            }))),
            "trigram" => Ok(Tokenizer::Trigram),
            _ => Err(Error::semantic(format!("no such tokenizer: {name}"))),
        }
    }
}

/// Parsed CREATE VIRTUAL TABLE ... USING fts5(...) arguments.
#[derive(Clone, Debug)]
pub(crate) struct FtsConfig {
    table: String,
    columns: Vec<FtsColumn>,
    tokenizer: Tokenizer,
    /// Prefix indexes (prefix='2 3'): tracked, though the in-memory index
    /// serves prefix queries directly.
    #[allow(dead_code)]
    prefix: Vec<u32>,
    /// content='' → contentless; content='tbl' → external content table.
    content: Option<String>,
    content_rowid: Option<String>,
    /// columnsize=0: doc sizes not tracked (bm25 then uses token counts).
    _columnsize: bool,
}

impl FtsConfig {
    /// Parse the module args (argv[3..] in SQLite terms).
    fn parse(table: &str, args: &[String]) -> Result<FtsConfig> {
        let mut columns: Vec<FtsColumn> = Vec::new();
        let mut tokenizer = Tokenizer::Unicode61 {
            remove_diacritics: 1,
            tokenchars: Vec::new(),
            separators: Vec::new(),
        };
        let mut prefix = Vec::new();
        let mut content: Option<String> = None;
        let mut content_rowid: Option<String> = None;
        let mut columnsize = true;
        let mut i = 0usize;
        while i < args.len() {
            let arg = args[i].trim().to_string();
            if arg.is_empty() {
                i += 1;
                continue;
            }
            // Option arguments arrive in either form: a single `k=v`
            // entry, or the parser's split pair ("k", "v") — SQLite's
            // argv protocol delivers key=value as TWO entries.
            let one_entry: Option<(String, String)> =
                split_option(&arg).map(|(k, v)| (k.to_string(), v.to_string()));
            let two_entry: Option<(String, String)> = if one_entry.is_none()
                && is_option_name(&arg)
                && i + 1 < args.len()
                && !is_option_name(args[i + 1].trim())
            {
                Some((arg.clone(), args[i + 1].clone()))
            } else {
                None
            };
            let single = one_entry.is_some();
            if let Some((k, v)) = one_entry.or(two_entry) {
                let consumed = if single { 1 } else { 2 };
                i += consumed;
                let k = k.to_ascii_lowercase();
                let v = v.trim().to_string();
                match k.as_str() {
                    "tokenize" => tokenizer = Tokenizer::parse(&v)?,
                    "prefix" => {
                        for p in v.split_whitespace() {
                            let n: u32 = p.parse().map_err(|_| {
                                Error::semantic(format!("malformed prefix= option: {v}"))
                            })?;
                            prefix.push(n);
                        }
                    }
                    "content" => {
                        content = if v.is_empty() {
                            Some(String::new()) // contentless
                        } else {
                            Some(unquote_ident(&v))
                        }
                    }
                    "content_rowid" => content_rowid = Some(unquote_ident(&v)),
                    "contentless_delete" => { /* accepted */ }
                    "columnsize" | "detail" => {
                        if k.eq_ignore_ascii_case("columnsize") {
                            columnsize = v != "0";
                        }
                        // detail=none|column accepted; positions always kept
                    }
                    "tokendata" => { /* accepted */ }
                    _ => return Err(Error::semantic(format!("unrecognized option: {k}"))),
                }
                continue;
            }
            i += 1;
            // Column argument: name [UNINDEXED] or quoted 'name'
            let (name, unindexed) = parse_column_arg(&arg)?;
            if columns.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
                return Err(Error::semantic(format!("duplicate column name: {name}")));
            }
            columns.push(FtsColumn { name, unindexed });
        }
        if columns.is_empty() {
            return Err(Error::semantic("no columns declared for fts5 table"));
        }
        Ok(FtsConfig {
            table: table.to_string(),
            columns,
            tokenizer,
            prefix,
            content,
            content_rowid,
            _columnsize: columnsize,
        })
    }

    fn contentless(&self) -> bool {
        self.content.as_deref() == Some("")
    }

    fn external(&self) -> Option<&str> {
        match &self.content {
            Some(c) if !c.is_empty() => Some(c),
            _ => None,
        }
    }
}

/// The fts5 option names (an arg starting with one of these, followed by
/// a value entry, is an option — not a column).
fn is_option_name(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        "tokenize"
            | "prefix"
            | "content"
            | "content_rowid"
            | "contentless_delete"
            | "columnsize"
            | "detail"
            | "tokendata"
    )
}

/// Split `name = value` (the value may be single- or double-quoted).
fn split_option(arg: &str) -> Option<(&str, &str)> {
    let pos = arg.find('=')?;
    let name = &arg[..pos];
    let name_t = name.trim();
    if name_t.is_empty()
        || !name_t
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return None;
    }
    Some((name_t, &arg[pos + 1..]))
}

fn unquote_ident(v: &str) -> String {
    let v = v.trim();
    let mut out = v.to_string();
    if out.len() >= 2 {
        let bytes = out.as_bytes();
        let first = bytes[0];
        let last = bytes[out.len() - 1];
        if (first == b'\'' && last == b'\'')
            || (first == b'"' && last == b'"')
            || (first == b'`' && last == b'`')
            || (first == b'[' && last == b']')
        {
            out = out[1..out.len() - 1].to_string();
        }
    }
    out
}

/// Parse one column argument: `name`, `'name'`, `name UNINDEXED`,
/// `"name" UNINDEXED` (the parser hands us each comma-separated arg with
/// embedded quotes preserved when the arg was a quoted string).
fn parse_column_arg(arg: &str) -> Result<(String, bool)> {
    let mut s = arg.trim();
    let mut unindexed = false;
    if let Some(rest) = strip_suffix_ci(s, " UNINDEXED") {
        s = rest.trim();
        unindexed = true;
    } else if let Some(rest) = strip_suffix_ci(s, " unindexed") {
        s = rest.trim();
        unindexed = true;
    }
    let name = unquote_ident(s);
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '"')
    {
        return Err(Error::semantic(format!(
            "unrecognized column declaration: {arg}"
        )));
    }
    Ok((name, unindexed))
}

fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    if s.len() >= suffix.len() && s[s.len() - suffix.len()..].eq_ignore_ascii_case(suffix) {
        Some(&s[..s.len() - suffix.len()])
    } else {
        None
    }
}

// ============================================================================
// Tokenizer
// ============================================================================

/// One token occurrence: byte span in the source text + 1-based position.
#[derive(Clone, Debug)]
struct Tok {
    start: u32,
    end: u32,
    /// 1-based token position within the column.
    pos: u16,
    text: String,
}

/// Tokenize `text` into folded tokens with byte spans and positions.
fn tokenize(cfg: &FtsConfig, text: &str, col: usize) -> Vec<Tok> {
    if col >= cfg.columns.len() || cfg.columns[col].unindexed {
        return Vec::new();
    }
    match &cfg.tokenizer {
        Tokenizer::Trigram => {
            // Every 3-char window of the case-folded text.
            let folded: String = text.chars().flat_map(|c| c.to_lowercase()).collect();
            let chars: Vec<(usize, char)> = folded.char_indices().collect();
            let mut out = Vec::new();
            if chars.len() >= 3 {
                for w in chars.windows(3) {
                    let start = w[0].0 as u32;
                    let end = (w[2].0 + w[2].1.len_utf8()) as u32;
                    let t: String = [w[0].1, w[1].1, w[2].1].iter().collect();
                    out.push(Tok {
                        start,
                        end,
                        pos: (out.len() + 1) as u16,
                        text: t,
                    });
                }
            }
            out
        }
        base => {
            let toks = tokenize_base(base, text);
            if let Tokenizer::Porter(inner) = base {
                // porter stems the unicode61 output
                let _ = inner;
                toks.into_iter()
                    .map(|t| Tok {
                        text: porter_stem(&t.text),
                        start: t.start,
                        end: t.end,
                        pos: t.pos,
                    })
                    .collect()
            } else {
                toks
            }
        }
    }
}

fn tokenize_base(tok: &Tokenizer, text: &str) -> Vec<Tok> {
    let (tokenchars, separators, remove_diacritics) = match tok {
        Tokenizer::Unicode61 {
            remove_diacritics,
            tokenchars,
            separators,
        } => (tokenchars.clone(), separators.clone(), *remove_diacritics),
        _ => (Vec::new(), Vec::new(), 1),
    };
    let mut out: Vec<Tok> = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 0u32;
    let mut pending_end = 0u32;
    for (idx, ch) in text.char_indices() {
        let idx = idx as u32;
        let is_sep = match tok {
            Tokenizer::Ascii => !ch.is_ascii_alphanumeric(),
            _ => {
                let mut c = ch;
                if remove_diacritics > 0 {
                    c = strip_diacritic(ch, remove_diacritics == 2);
                }
                let tc = c.is_alphanumeric() || tokenchars.contains(&ch);
                let sep = separators.contains(&ch);
                // An explicit separator forces a boundary; tokenchars
                // join even non-alphanumerics.
                if sep {
                    true
                } else {
                    !tc
                }
            }
        };
        if is_sep {
            if !cur.is_empty() {
                out.push(Tok {
                    start: cur_start,
                    end: pending_end,
                    pos: (out.len() + 1) as u16,
                    text: std::mem::take(&mut cur),
                });
            }
        } else {
            if cur.is_empty() {
                cur_start = idx;
            }
            let c = match tok {
                Tokenizer::Ascii => ch.to_ascii_lowercase(),
                _ => {
                    let mut c = ch;
                    if remove_diacritics > 0 {
                        c = strip_diacritic(ch, remove_diacritics == 2);
                    }
                    c
                }
            };
            for lc in c.to_lowercase() {
                cur.push(lc);
            }
            pending_end = idx + ch.len_utf8() as u32;
        }
    }
    if !cur.is_empty() {
        out.push(Tok {
            start: cur_start,
            end: pending_end,
            pos: (out.len() + 1) as u16,
            text: cur,
        });
    }
    out
}

/// Remove the common diacritics (Latin-1 Supplement + Latin Extended-A
/// precomposed characters). Mode 1 covers the "common" set SQLite's
/// unicode61 handles by default; mode 2 the extended one.
fn strip_diacritic(c: char, extended: bool) -> char {
    if c.is_ascii() {
        return c;
    }
    match c {
        'à'..='å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'ď' | 'đ' => 'd',
        'è'..='ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => 'g',
        'ĥ' | 'ħ' => 'h',
        'ì'..='ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => 'i',
        'ĵ' => 'j',
        'ķ' => 'k',
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => 'l',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò'..='ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ŕ' | 'ŗ' | 'ř' => 'r',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ţ' | 'ť' | 'ŧ' => 't',
        'ù'..='ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ŵ' => 'w',
        'ý' | 'ÿ' | 'ŷ' => 'y',
        'ź' | 'ż' | 'ž' => 'z',
        _ if extended => {
            // Extended mode: try the remaining Latin Extended ranges via
            // a canonical decomposition table entry point.
            match c as u32 {
                0x1E00..=0x1EFF => strip_latin_ext_additional(c),
                _ => c,
            }
        }
        _ => c,
    }
}

fn strip_latin_ext_additional(c: char) -> char {
    // Latin Extended Additional: mostly base + dot/ring/caron variants.
    match c {
        'ḁ' => 'a',
        'ḃ' | 'ḅ' | 'ḇ' => 'b',
        'ḉ' => 'c',
        'ḋ' | 'ḍ' | 'ḏ' | 'ḑ' | 'ḓ' => 'd',
        'ḕ' | 'ḗ' | 'ḙ' | 'ḛ' | 'ḝ' => 'e',
        'ḟ' => 'f',
        'ḡ' => 'g',
        'ḣ' | 'ḥ' | 'ḧ' | 'ḩ' | 'ḫ' => 'h',
        'ḭ' | 'ḯ' => 'i',
        'ḱ' | 'ḳ' | 'ḵ' => 'k',
        'ḷ' | 'ḹ' | 'ḻ' | 'ḽ' => 'l',
        'ḿ' | 'ṁ' | 'ṃ' => 'm',
        'ṅ' | 'ṇ' | 'ṉ' | 'ṋ' => 'n',
        'ṍ' | 'ṏ' | 'ṑ' | 'ṓ' | 'ṕ' => 'o',
        'ṗ' => 'p',
        'ṙ' | 'ṛ' | 'ṝ' | 'ṟ' => 'r',
        'ṡ' | 'ṣ' | 'ṥ' | 'ṧ' | 'ṩ' => 's',
        'ṫ' | 'ṭ' | 'ṯ' | 'ṱ' => 't',
        'ṳ' | 'ṵ' | 'ṷ' | 'ṹ' | 'ṻ' => 'u',
        'ṽ' | 'ṿ' => 'v',
        'ẁ' | 'ẃ' | 'ẅ' | 'ẇ' | 'ẉ' | 'ẋ' | 'ẍ' | 'ẏ' => {
            c.to_lowercase().next().unwrap_or(c)
        }
        'ẑ' | 'ẓ' | 'ẕ' => 'z',
        _ => c,
    }
}

/// The Porter stemming algorithm (the classic 1980 paper's suffix-strip
/// procedure, as used by SQLite's porter tokenizer).
fn porter_stem(w: &str) -> String {
    if w.len() <= 2 {
        return w.to_string();
    }
    let b: Vec<u8> = w.bytes().collect();
    let mut s = porter_step1(&b);
    s = porter_step2(&s);
    s = porter_step3(&s);
    s = porter_step4(&s);
    s = porter_step5(&s);
    String::from_utf8(s).unwrap_or_else(|_| w.to_string())
}

fn is_cons(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => {
            if i == 0 {
                true
            } else {
                !is_cons(b, i - 1)
            }
        }
        _ => true,
    }
}

fn m_measure(b: &[u8], end: usize) -> usize {
    // [C](VC)^m[V]
    let mut i = 0;
    while i < end && is_cons(b, i) {
        i += 1;
    }
    let mut m = 0;
    while i < end {
        while i < end && !is_cons(b, i) {
            i += 1;
        }
        if i >= end {
            break;
        }
        m += 1;
        while i < end && is_cons(b, i) {
            i += 1;
        }
    }
    m
}

fn has_vowel(b: &[u8], end: usize) -> bool {
    (0..end).any(|i| !is_cons(b, i))
}

fn ends_double_cons(b: &[u8]) -> bool {
    let n = b.len();
    n >= 2 && b[n - 1] == b[n - 2] && is_cons(b, n - 1)
}

fn cvc(b: &[u8]) -> bool {
    // consonant-vowel-consonant ending, last consonant not w/x/y
    let n = b.len();
    n >= 3
        && is_cons(b, n - 3)
        && !is_cons(b, n - 2)
        && is_cons(b, n - 1)
        && !matches!(b[n - 1], b'w' | b'x' | b'y')
}

fn replace_suffix(b: &[u8], suf: &str, rep: &str) -> Vec<u8> {
    let mut v = b[..b.len() - suf.len()].to_vec();
    v.extend_from_slice(rep.as_bytes());
    v
}

fn porter_step1(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    let s: Vec<u8> = if n > 4 && b.ends_with(b"sses") {
        replace_suffix(b, "sses", "ss")
    } else if n > 3 && b.ends_with(b"ies") {
        replace_suffix(b, "ies", "i")
    } else if n > 2 && b.ends_with(b"ss") {
        b.to_vec()
    } else if n > 1 && b.ends_with(b"s") && !b.ends_with(b"ss") {
        b[..n - 1].to_vec()
    } else {
        b.to_vec()
    };
    let n = s.len();
    let s: Vec<u8> = if n > 3 && s.ends_with(b"eed") {
        if m_measure(&s, n - 3) > 0 {
            replace_suffix(&s, "eed", "ee")
        } else {
            s
        }
    } else if n > 2 && (s.ends_with(b"ed") && has_vowel(&s, n - 2)) {
        let mut v = s[..n - 2].to_vec();
        v = porter_ed_cleanup(&v);
        v
    } else if n > 2 && (s.ends_with(b"ing") && has_vowel(&s, n - 3)) {
        let mut v = s[..n - 3].to_vec();
        v = porter_ed_cleanup(&v);
        v
    } else {
        s
    };
    let n = s.len();
    let s: Vec<u8> = if n > 3 && s.ends_with(b"y") && has_vowel(&s, n - 1) {
        let mut v = s[..n - 1].to_vec();
        v.push(b'i');
        v
    } else {
        s
    };
    s
}

fn porter_ed_cleanup(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    if n >= 2 {
        if b.ends_with(b"at") {
            let mut v = b.to_vec();
            v.push(b'e');
            return v;
        }
        if b.ends_with(b"bl") {
            let mut v = b.to_vec();
            v.push(b'e');
            return v;
        }
        if b.ends_with(b"iz") {
            let mut v = b.to_vec();
            v.push(b'e');
            return v;
        }
    }
    if ends_double_cons(b) && !matches!(b[n - 1], b'l' | b's' | b'z') {
        return b[..n - 1].to_vec();
    }
    if m_measure(b, n) == 1 && cvc(b) {
        let mut v = b.to_vec();
        v.push(b'e');
        return v;
    }
    b.to_vec()
}

fn porter_step2(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    if n < 4 {
        return b.to_vec();
    }
    let pairs: &[(&str, &str)] = &[
        ("ational", "ate"),
        ("tional", "tion"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("izer", "ize"),
        ("abli", "able"),
        ("alli", "al"),
        ("entli", "ent"),
        ("eli", "e"),
        ("ousli", "ous"),
        ("ization", "ize"),
        ("ation", "ate"),
        ("ator", "ate"),
        ("alism", "al"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("aliti", "al"),
        ("iviti", "ive"),
        ("biliti", "ble"),
    ];
    for (suf, rep) in pairs {
        if b.ends_with(suf.as_bytes()) {
            if m_measure(b, n - suf.len()) > 0 {
                return replace_suffix(b, suf, rep);
            }
            return b.to_vec();
        }
    }
    b.to_vec()
}

fn porter_step3(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    let pairs: &[(&str, &str)] = &[
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (suf, rep) in pairs {
        if b.ends_with(suf.as_bytes()) {
            if m_measure(b, n - suf.len()) > 0 {
                return replace_suffix(b, suf, rep);
            }
            return b.to_vec();
        }
    }
    b.to_vec()
}

fn porter_step4(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    let sufs: &[&str] = &[
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion",
        "ou", "ism", "ate", "iti", "ous", "ive", "ize",
    ];
    for suf in sufs {
        if b.ends_with(suf.as_bytes()) {
            let stem_len = n - suf.len();
            if *suf == "ion" {
                if stem_len > 0
                    && matches!(b[stem_len - 1], b's' | b't')
                    && m_measure(b, stem_len) > 1
                {
                    return b[..stem_len].to_vec();
                }
                return b.to_vec();
            }
            if m_measure(b, stem_len) > 1 {
                return b[..stem_len].to_vec();
            }
            return b.to_vec();
        }
    }
    b.to_vec()
}

fn porter_step5(b: &[u8]) -> Vec<u8> {
    let n = b.len();
    if n >= 2 && b.ends_with(b"e") {
        let m = m_measure(b, n - 1);
        if m > 1 {
            return b[..n - 1].to_vec();
        }
        if m == 1 && !cvc(&b[..n - 1]) {
            return b[..n - 1].to_vec();
        }
    }
    if n > 1 && b.ends_with(b"ll") && m_measure(b, n - 1) > 1 {
        return b[..n - 1].to_vec();
    }
    b.to_vec()
}

// ============================================================================
// Query parsing (fts5_expr grammar)
// ============================================================================

/// One term of a phrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueryTerm {
    text: String,
    prefix: bool,
    /// ^ anchor: the term must be the column's first token.
    initial: bool,
}

/// A phrase: consecutive terms, restricted to a set of columns
/// (None = all columns).
#[derive(Clone, Debug)]
pub(crate) struct Phrase {
    terms: Vec<QueryTerm>,
    cols: Option<Vec<usize>>,
}

/// The parsed query tree.
#[derive(Clone, Debug)]
pub(crate) enum Query {
    Phrase(Phrase),
    And(Box<Query>, Box<Query>),
    Or(Box<Query>, Box<Query>),
    Not(Box<Query>, Box<Query>),
    /// NEAR(phrases, distance): all phrases within `dist` tokens of each
    /// other in the SAME column.
    Near(Vec<Phrase>, u32),
}

/// Lexer for the query string.
struct QueryLexer<'a> {
    s: &'a [u8],
    pos: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum QTok {
    /// A bareword (may carry a trailing * / leading ^).
    Word(String, bool, bool),
    /// A quoted string ("" escape already unfolded).
    Quoted(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Caret,
    Star,
}

impl<'a> QueryLexer<'a> {
    fn new(s: &'a str) -> Self {
        QueryLexer {
            s: s.as_bytes(),
            pos: 0,
        }
    }
    fn next(&mut self) -> Result<Option<QTok>> {
        while self.pos < self.s.len() && (self.s[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
        if self.pos >= self.s.len() {
            return Ok(None);
        }
        let c = self.s[self.pos] as char;
        match c {
            '(' => {
                self.pos += 1;
                Ok(Some(QTok::LParen))
            }
            ')' => {
                self.pos += 1;
                Ok(Some(QTok::RParen))
            }
            '{' => {
                self.pos += 1;
                Ok(Some(QTok::LBrace))
            }
            '}' => {
                self.pos += 1;
                Ok(Some(QTok::RBrace))
            }
            ':' => {
                self.pos += 1;
                Ok(Some(QTok::Colon))
            }
            ',' => {
                self.pos += 1;
                Ok(Some(QTok::Comma))
            }
            '^' => {
                self.pos += 1;
                Ok(Some(QTok::Caret))
            }
            '"' => {
                // Quoted string, "" is an escaped quote.
                self.pos += 1;
                let mut out = String::new();
                let mut closed = false;
                while self.pos < self.s.len() {
                    if self.s[self.pos] == b'"' {
                        if self.pos + 1 < self.s.len() && self.s[self.pos + 1] == b'"' {
                            out.push('"');
                            self.pos += 2;
                            continue;
                        }
                        self.pos += 1;
                        closed = true;
                        break;
                    }
                    out.push(self.s[self.pos] as char);
                    self.pos += 1;
                }
                if !closed {
                    return Err(Error::semantic("unterminated string in fts5 query"));
                }
                Ok(Some(QTok::Quoted(out)))
            }
            _ => {
                // Bareword: up to whitespace or a structural char.
                let start = self.pos;
                let mut word = String::new();
                while self.pos < self.s.len() {
                    let ch = self.s[self.pos] as char;
                    if ch.is_whitespace() || matches!(ch, '(' | ')' | '{' | '}' | ':' | ',' | '"') {
                        break;
                    }
                    if ch == '*' {
                        // Every star is its own token: `abc*` = Word
                        // ("abc") + Star (trailing prefix marker); a lone
                        // leading star falls to the empty-word case below.
                        break;
                    }
                    // Case PRESERVED: fts5 operators are uppercase-only
                    // (lowercase "or" is a term); folding happens at term
                    // construction.
                    word.push(ch);
                    self.pos += 1;
                }
                if word.is_empty() {
                    // A lone '*' or unexpected char — consume it as a star.
                    if c == '*' {
                        self.pos += 1;
                        return Ok(Some(QTok::Star));
                    }
                    return Err(Error::semantic(format!(
                        "fts5 syntax error near {:?}",
                        self.s[self.pos] as char
                    )));
                }
                let _ = start;
                Ok(Some(QTok::Word(word, false, false)))
            }
        }
    }
}

/// Parse an fts5 query string against the table's columns.
pub(crate) fn parse_query(cfg: &FtsConfig, q: &str) -> Result<Query> {
    let mut lx = QueryLexer::new(q);
    let mut toks = Vec::new();
    while let Some(t) = lx.next()? {
        toks.push(t);
    }
    if toks.is_empty() {
        return Err(Error::semantic("fts5 query is empty"));
    }
    let mut p = QParser { toks, pos: 0, cfg };
    let e = p.parse_or()?;
    if p.pos != p.toks.len() {
        return Err(Error::semantic("fts5 syntax error"));
    }
    Ok(e)
}

struct QParser<'a> {
    toks: Vec<QTok>,
    pos: usize,
    cfg: &'a FtsConfig,
}

impl<'a> QParser<'a> {
    fn peek(&self) -> Option<&QTok> {
        self.toks.get(self.pos)
    }
    fn is_kw(&self, w: &str) -> bool {
        match self.peek() {
            Some(QTok::Word(s, _, _)) => s == w,
            _ => false,
        }
    }

    /// orlist := andlist (OR andlist)*
    fn parse_or(&mut self) -> Result<Query> {
        let mut left = self.parse_and()?;
        while self.is_kw("OR") {
            self.pos += 1;
            let right = self.parse_and()?;
            left = Query::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// andlist := notlist ( [AND] notlist )*
    fn parse_and(&mut self) -> Result<Query> {
        let mut left = self.parse_not()?;
        loop {
            if self.is_kw("AND") {
                self.pos += 1;
                let right = self.parse_not()?;
                left = Query::And(Box::new(left), Box::new(right));
                continue;
            }
            // Implicit AND: the next token can START a primary.
            if self.starts_primary() {
                let right = self.parse_not()?;
                left = Query::And(Box::new(left), Box::new(right));
                continue;
            }
            break;
        }
        Ok(left)
    }

    fn starts_primary(&self) -> bool {
        match self.peek() {
            Some(QTok::Word(w, _, _)) => w != "AND" && w != "OR" && w != "NOT" && w != "NEAR",
            Some(QTok::Quoted(_)) | Some(QTok::LParen) | Some(QTok::LBrace) | Some(QTok::Caret) => {
                true
            }
            _ => false,
        }
    }

    /// notlist := primary (NOT primary)*
    fn parse_not(&mut self) -> Result<Query> {
        let mut left = self.parse_primary()?;
        while self.is_kw("NOT") {
            self.pos += 1;
            let right = self.parse_primary()?;
            left = Query::Not(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    /// primary := [colfilter] (phrase | '(' expr ')') | NEAR(...)
    fn parse_primary(&mut self) -> Result<Query> {
        if self.is_kw("NEAR") {
            self.pos += 1;
            return self.parse_near();
        }
        // Column filter?
        let cols = self.try_colfilter()?;
        if self.is_kw("NEAR") && cols.is_some() {
            // NEAR with a column filter applies it to every phrase.
            self.pos += 1;
            let Query::Near(phrases, d) = self.parse_near()? else {
                unreachable!()
            };
            let phrases = phrases
                .into_iter()
                .map(|mut p| {
                    p.cols = cols.clone();
                    p
                })
                .collect();
            return Ok(Query::Near(phrases, d));
        }
        match self.peek().cloned() {
            Some(QTok::LParen) => {
                self.pos += 1;
                let e = self.parse_or()?;
                match self.peek() {
                    Some(QTok::RParen) => {
                        self.pos += 1;
                    }
                    _ => return Err(Error::semantic("fts5 syntax error: expected )")),
                }
                Ok(e)
            }
            Some(QTok::Quoted(s)) => {
                self.pos += 1;
                Ok(Query::Phrase(Phrase {
                    terms: phrase_terms(&s, false),
                    cols,
                }))
            }
            Some(QTok::Caret) => {
                self.pos += 1;
                let terms = self.parse_bareword_phrase(true)?;
                Ok(Query::Phrase(Phrase { terms, cols }))
            }
            Some(QTok::Word(w, _, _)) => {
                self.pos += 1;
                let mut terms = Vec::new();
                let initial = false;
                terms.push(QueryTerm {
                    text: w.to_lowercase(),
                    prefix: false,
                    initial,
                });
                // Adjacent barewords/quotes extend the phrase.
                loop {
                    match self.peek().cloned() {
                        Some(QTok::Word(w2, _, _))
                            if w2 != "AND" && w2 != "OR" && w2 != "NOT" && w2 != "NEAR" =>
                        {
                            self.pos += 1;
                            terms.push(QueryTerm {
                                text: w2.to_lowercase(),
                                prefix: false,
                                initial: false,
                            });
                        }
                        Some(QTok::Star) => {
                            self.pos += 1;
                            if let Some(last) = terms.last_mut() {
                                last.prefix = true;
                            }
                        }
                        Some(QTok::Quoted(s)) => {
                            self.pos += 1;
                            terms.extend(phrase_terms(&s, false));
                        }
                        Some(QTok::Caret) => {
                            self.pos += 1;
                            let more = self.parse_bareword_phrase(true)?;
                            terms.extend(more);
                        }
                        _ => break,
                    }
                }
                Ok(Query::Phrase(Phrase { terms, cols }))
            }
            _ => Err(Error::semantic("fts5 syntax error")),
        }
    }

    /// One bareword after ^ (or a ^-anchored word inside a phrase).
    fn parse_bareword_phrase(&mut self, initial: bool) -> Result<Vec<QueryTerm>> {
        match self.peek().cloned() {
            Some(QTok::Word(w, _, _)) => {
                self.pos += 1;
                Ok(vec![QueryTerm {
                    text: w.to_lowercase(),
                    prefix: false,
                    initial,
                }])
            }
            Some(QTok::Quoted(s)) => {
                self.pos += 1;
                Ok(phrase_terms(&s, initial))
            }
            _ => Err(Error::semantic("fts5 syntax error after ^")),
        }
    }

    /// NEAR '(' (phrase)+ [',' N] ')' — phrases separated by whitespace.
    fn parse_near(&mut self) -> Result<Query> {
        match self.peek() {
            Some(QTok::LParen) => {
                self.pos += 1;
            }
            _ => return Err(Error::semantic("NEAR must be followed by (")),
        }
        let mut phrases = Vec::new();
        let mut dist: u32 = 10;
        loop {
            match self.peek().cloned() {
                Some(QTok::Word(w, _, _)) if w != "NEAR" => {
                    self.pos += 1;
                    let mut terms = vec![QueryTerm {
                        text: w.to_lowercase(),
                        prefix: false,
                        initial: false,
                    }];
                    // Whitespace-separated words inside NEAR are
                    // SEPARATE phrases (SQLite: NEAR(a b) = NEAR of the
                    // single-term phrases a and b); a quote/star suffix
                    // extends THIS word's phrase.
                    match self.peek().cloned() {
                        Some(QTok::Quoted(s)) => {
                            self.pos += 1;
                            terms.extend(phrase_terms(&s, false));
                        }
                        Some(QTok::Star) => {
                            self.pos += 1;
                            if let Some(last) = terms.last_mut() {
                                last.prefix = true;
                            }
                        }
                        _ => {}
                    }
                    phrases.push(Phrase { terms, cols: None });
                }
                Some(QTok::Quoted(s)) => {
                    self.pos += 1;
                    phrases.push(Phrase {
                        terms: phrase_terms(&s, false),
                        cols: None,
                    });
                }
                Some(QTok::Comma) => {
                    self.pos += 1;
                    // The distance: an integer (possibly negative → 0).
                    match self.peek().cloned() {
                        Some(QTok::Word(n, _, _)) => {
                            self.pos += 1;
                            dist = n.parse::<i64>().unwrap_or(10).max(0) as u32;
                        }
                        _ => return Err(Error::semantic("NEAR distance expected")),
                    }
                }
                Some(QTok::RParen) => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(Error::semantic("fts5 syntax error inside NEAR")),
            }
        }
        if phrases.len() < 2 {
            return Err(Error::semantic("NEAR requires at least two phrases"));
        }
        Ok(Query::Near(phrases, dist))
    }

    /// Consume `col:` or `{c1 c2}:` if present.
    fn try_colfilter(&mut self) -> Result<Option<Vec<usize>>> {
        // Single-column: Word Colon
        if let Some(QTok::Word(w, _, _)) = self.peek().cloned() {
            if matches!(self.toks.get(self.pos + 1), Some(QTok::Colon)) {
                let col = self
                    .cfg
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&w));
                let Some(_) = col else {
                    return Err(Error::semantic(format!("no such column: {w}")));
                };
                self.pos += 2;
                return Ok(Some(vec![col.unwrap()]));
            }
        }
        // Multi-column: LBrace (Word)+ RBrace Colon
        if matches!(self.peek(), Some(QTok::LBrace)) {
            let save = self.pos;
            self.pos += 1;
            let mut cols = Vec::new();
            while let Some(QTok::Word(w, _, _)) = self.peek().cloned() {
                match self
                    .cfg
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&w))
                {
                    Some(i) => cols.push(i),
                    None => {
                        return Err(Error::semantic(format!("no such column: {w}")));
                    }
                }
                self.pos += 1;
            }
            if matches!(self.peek(), Some(QTok::RBrace))
                && matches!(self.toks.get(self.pos + 1), Some(QTok::Colon))
                && !cols.is_empty()
            {
                self.pos += 2;
                return Ok(Some(cols));
            }
            self.pos = save;
        }
        Ok(None)
    }
}

/// Split a quoted string into phrase terms (the string is already
/// case-folded by the lexer; re-tokenize it as whitespace-separated
/// words — a quoted phrase's words are exact tokens).
fn phrase_terms(s: &str, initial: bool) -> Vec<QueryTerm> {
    s.split_whitespace()
        .enumerate()
        .map(|(i, w)| QueryTerm {
            text: w.to_lowercase(),
            prefix: false,
            initial: initial && i == 0,
        })
        .collect()
}

// ============================================================================
// The inverted index
// ============================================================================

/// The text form of a value for tokenization (SQLite converts every
/// stored class to text for the tokenizer; NULL and BLOB contribute no
/// terms).
fn value_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Text(t)) => t.as_str().to_string(),
        Some(Value::Integer(i)) => i.to_string(),
        Some(Value::Real(f)) => format!("{f}"),
        _ => String::new(),
    }
}

/// Positions of one term's occurrences in one row: (column, positions).
#[derive(Clone, Debug, Default)]
struct Posting {
    cols: Vec<(u8, Vec<u16>)>,
}

impl Posting {
    fn positions_in(&self, col: usize) -> &[u16] {
        for (c, p) in &self.cols {
            if *c as usize == col {
                return p;
            }
        }
        &[]
    }
    fn any_col(&self) -> bool {
        self.cols.iter().any(|(_, p)| !p.is_empty())
    }
}

/// One indexed document: (column values, per-column token byte spans,
/// the row's distinct terms — the O(row-terms) unindex fast path).
type DocEntry = (Vec<Value>, Vec<Vec<(u32, u32)>>, Vec<String>);

/// The table's derived state: documents + inverted index.
struct FtsIndex {
    /// rowid → (column values, per-column token byte spans, the row's
    /// distinct terms — the O(row-terms) unindex fast path).
    docs: BTreeMap<i64, DocEntry>,
    /// term → rowid → positions.
    postings: HashMap<String, BTreeMap<i64, Posting>>,
    /// Per-row per-column token counts (bm25's D and column weights).
    doc_len: HashMap<i64, Vec<u32>>,
    total_tokens: u64,
}

impl FtsIndex {
    fn new() -> Self {
        FtsIndex {
            docs: BTreeMap::new(),
            postings: HashMap::new(),
            doc_len: HashMap::new(),
            total_tokens: 0,
        }
    }

    fn index_row(&mut self, cfg: &FtsConfig, rowid: i64, values: &[Value]) {
        self.unindex_row(cfg, rowid);
        let mut spans_per_col = Vec::with_capacity(cfg.columns.len());
        let mut lens = Vec::with_capacity(cfg.columns.len());
        let mut row_terms: Vec<String> = Vec::new();
        for (ci, _) in cfg.columns.iter().enumerate() {
            let text = value_text(values.get(ci));
            let toks = tokenize(cfg, &text, ci);
            lens.push(toks.len() as u32);
            spans_per_col.push(toks.iter().map(|t| (t.start, t.end)).collect());
            let mut by_term: HashMap<&str, Vec<u16>> = HashMap::new();
            for t in &toks {
                by_term.entry(t.text.as_str()).or_default().push(t.pos);
            }
            for (term, poss) in by_term {
                let entry = self
                    .postings
                    .entry(term.to_string())
                    .or_default()
                    .entry(rowid)
                    .or_default();
                entry.cols.push((ci as u8, poss));
                row_terms.push(term.to_string());
            }
        }
        let n: u32 = lens.iter().sum();
        self.total_tokens += n as u64;
        self.doc_len.insert(rowid, lens);
        self.docs
            .insert(rowid, (values.to_vec(), spans_per_col, row_terms));
    }

    fn unindex_row(&mut self, cfg: &FtsConfig, rowid: i64) {
        let Some((_, _, row_terms)) = self.docs.remove(&rowid) else {
            return;
        };
        if let Some(lens) = self.doc_len.remove(&rowid) {
            self.total_tokens -= lens.iter().sum::<u32>() as u64;
        }
        for term in &row_terms {
            if let Some(rows) = self.postings.get_mut(term) {
                rows.remove(&rowid);
                if rows.is_empty() {
                    self.postings.remove(term);
                }
            }
        }
        let _ = cfg;
    }

    /// Rows containing `term` (exact), with positions.
    fn rows_for_term(&self, term: &str) -> Option<&BTreeMap<i64, Posting>> {
        self.postings.get(term)
    }

    /// Rows containing any term with the given prefix, merged.
    fn rows_for_prefix(&self, prefix: &str) -> BTreeMap<i64, Vec<&Posting>> {
        let mut out: BTreeMap<i64, Vec<&Posting>> = BTreeMap::new();
        for (t, rows) in &self.postings {
            if t.starts_with(prefix) {
                for (rid, p) in rows {
                    if p.any_col() {
                        out.entry(*rid).or_default().push(p);
                    }
                }
            }
        }
        out
    }

    fn n_rows(&self) -> i64 {
        self.docs.len() as i64
    }
}

// ============================================================================
// The FTS5 virtual table
// ============================================================================

// The table's mutable state, shared between the instance and its cursors
// (cursors outlive the `with_table` borrow — an Arc<Mutex> gives them a
// stable view while DML mutates between scans).
struct FtsState {
    cfg: FtsConfig,
    index: FtsIndex,
    /// The query of the most recent filter() (rank/highlight/snippet all
    /// score against it — SQLite binds aux calls to the driving cursor).
    active_query: Option<Query>,
    /// Per-column bm25 weights (rank MATCH 'bm25(...)').
    weights: Vec<f64>,
}

impl FtsState {
    fn n_user_cols(&self) -> usize {
        self.cfg.columns.len()
    }

    fn eval_query(&self, q: &Query) -> BTreeMap<i64, Vec<PhraseHit>> {
        let mut out = BTreeMap::new();
        self.eval_into(q, &mut out);
        out
    }

    fn eval_into(&self, q: &Query, out: &mut BTreeMap<i64, Vec<PhraseHit>>) {
        match q {
            Query::Phrase(p) => {
                for (rid, hits) in self.phrase_rows(p) {
                    out.entry(rid).or_default().extend(hits);
                }
            }
            Query::And(l, r) => {
                let mut left = BTreeMap::new();
                self.eval_into(l, &mut left);
                let mut right = BTreeMap::new();
                self.eval_into(r, &mut right);
                for (rid, hits) in left {
                    if let Some(rh) = right.remove(&rid) {
                        let mut all = hits;
                        all.extend(rh);
                        out.insert(rid, all);
                    }
                }
            }
            Query::Or(l, r) => {
                self.eval_into(l, out);
                self.eval_into(r, out);
            }
            Query::Not(l, r) => {
                let mut left = BTreeMap::new();
                self.eval_into(l, &mut left);
                let mut right = BTreeMap::new();
                self.eval_into(r, &mut right);
                for (rid, hits) in left {
                    if !right.contains_key(&rid) {
                        out.insert(rid, hits);
                    }
                }
            }
            Query::Near(phrases, dist) => {
                let mut per_phrase: Vec<BTreeMap<i64, Vec<PhraseHit>>> =
                    Vec::with_capacity(phrases.len());
                for p in phrases {
                    per_phrase.push(self.phrase_rows(p));
                }
                let mut cands: Vec<i64> = per_phrase[0].keys().copied().collect();
                for m in &per_phrase[1..] {
                    cands.retain(|rid| m.contains_key(rid));
                }
                'row: for rid in cands {
                    // NEAR is column-scoped: every phrase must hit ONE
                    // common column with positions within `dist`.
                    for ci in 0..self.n_user_cols() {
                        let mut first: Option<PhraseHit> = None;
                        let mut last: Option<PhraseHit> = None;
                        let mut all_here = true;
                        for m in &per_phrase {
                            match m[&rid].iter().find(|h| h.col == ci) {
                                Some(h) => {
                                    if first.is_none() {
                                        first = Some(h.clone());
                                    }
                                    last = Some(h.clone());
                                }
                                None => {
                                    all_here = false;
                                    break;
                                }
                            }
                        }
                        if !all_here {
                            continue;
                        }
                        let f = first.unwrap();
                        let l = last.unwrap();
                        let start = f.start.min(l.start);
                        let end = f.end.max(l.end);
                        if u32::from(end.saturating_sub(start)) <= *dist {
                            let mut hits = Vec::new();
                            for m in &per_phrase {
                                hits.extend(m[&rid].iter().filter(|h| h.col == ci).cloned());
                            }
                            out.entry(rid).or_default().extend(hits);
                            continue 'row;
                        }
                    }
                }
            }
        }
    }

    /// Rows containing the phrase, with per-row hit lists.
    fn phrase_rows(&self, p: &Phrase) -> BTreeMap<i64, Vec<PhraseHit>> {
        let mut out: BTreeMap<i64, Vec<PhraseHit>> = BTreeMap::new();
        if p.terms.is_empty() {
            return out;
        }
        let allowed: Option<Vec<usize>> = p.cols.clone();
        // Candidates: rows containing the first term.
        let t0 = &p.terms[0];
        let mut cands: HashSet<i64> = HashSet::new();
        if t0.prefix {
            for rid in self.index.rows_for_prefix(&t0.text).keys() {
                cands.insert(*rid);
            }
        } else if let Some(rows) = self.index.rows_for_term(&t0.text) {
            for (rid, po) in rows {
                if po.any_col() {
                    cands.insert(*rid);
                }
            }
        }
        for rid in cands {
            let col_range: Vec<usize> = match &allowed {
                Some(cs) => cs.clone(),
                None => (0..self.n_user_cols()).collect(),
            };
            let mut hits = Vec::new();
            for ci in col_range {
                if let Some(positions) = self.phrase_positions(rid, ci, &p.terms) {
                    for (s, e) in positions {
                        hits.push(PhraseHit {
                            col: ci,
                            start: s,
                            end: e,
                        });
                    }
                }
            }
            if !hits.is_empty() {
                out.insert(rid, hits);
            }
        }
        out
    }

    /// The (start, end) 1-based token positions of the phrase in
    /// `rowid`/`col`, or None when absent.
    fn phrase_positions(
        &self,
        rowid: i64,
        col: usize,
        terms: &[QueryTerm],
    ) -> Option<Vec<(u16, u16)>> {
        let mut per_term: Vec<Vec<u16>> = Vec::with_capacity(terms.len());
        for t in terms {
            let mut poss: Vec<u16> = Vec::new();
            if t.prefix {
                for (term, rows) in &self.index.postings {
                    if term.starts_with(&t.text) {
                        if let Some(p) = rows.get(&rowid) {
                            poss.extend(p.positions_in(col).iter().copied());
                        }
                    }
                }
            } else if let Some(rows) = self.index.rows_for_term(&t.text) {
                if let Some(p) = rows.get(&rowid) {
                    poss = p.positions_in(col).to_vec();
                }
            }
            if poss.is_empty() {
                return None;
            }
            poss.sort_unstable();
            poss.dedup();
            per_term.push(poss);
        }
        let first = &per_term[0];
        let mut out = Vec::new();
        for &start in first {
            let mut ok = true;
            for (i, positions) in per_term.iter().enumerate().skip(1) {
                if !positions.contains(&(start + i as u16)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                out.push((start, start + terms.len() as u16 - 1));
            }
        }
        if out.is_empty() || (terms[0].initial && !out.iter().any(|(s, _)| *s == 1)) {
            None
        } else {
            Some(out)
        }
    }

    /// BM25 (SQLite's fts5 formula; negative = better).
    fn bm25(&self, rowid: i64, weights: &[f64]) -> f64 {
        let Some(q) = &self.active_query else {
            return 0.0;
        };
        let n_rows = self.index.n_rows();
        if n_rows == 0 {
            return 0.0;
        }
        let d_total: u32 = self
            .index
            .doc_len
            .get(&rowid)
            .map(|l| l.iter().sum())
            .unwrap_or(0);
        let avgdl = self.index.total_tokens as f64 / n_rows as f64;
        if avgdl <= 0.0 {
            return 0.0;
        }
        const K1: f64 = 1.2;
        const B: f64 = 0.75;
        let phrases = collect_phrases(q);
        let mut score = 0.0f64;
        for p in &phrases {
            let rows = self.phrase_rows(p);
            let n_hit = rows.len() as i64;
            if n_hit == 0 {
                continue;
            }
            // SQLite's exact formula (fts5Bm25GetData):
            //   IDF = log((N - nHit + 0.5) / (nHit + 0.5)), min 1e-6.
            let mut idf = ((n_rows - n_hit) as f64 + 0.5) / (n_hit as f64 + 0.5);
            idf = idf.ln();
            if idf <= 0.0 {
                idf = 1e-6;
            }
            let mut freq = 0.0f64;
            if let Some(hits) = rows.get(&rowid) {
                for h in hits {
                    freq += weights.get(h.col).copied().unwrap_or(1.0);
                }
            }
            if freq == 0.0 {
                continue;
            }
            let dl = d_total as f64;
            score += idf * (freq * (K1 + 1.0)) / (freq + K1 * (1.0 - B + B * dl / avgdl));
        }
        -score
    }

    /// highlight(): the column's text with every active-query hit span
    /// wrapped.
    fn highlight(&self, rowid: i64, col: i64, pre: &str, post: &str) -> String {
        let Some((_, spans, _)) = self.index.docs.get(&rowid) else {
            return String::new();
        };
        if col < 0 || col as usize >= spans.len() {
            return String::new();
        }
        let ci = col as usize;
        let text = self.doc_text(rowid, ci);
        let spans_ci = &spans[ci];
        let mut ranges: Vec<(u32, u32)> = self
            .active_hits_in(rowid, ci)
            .iter()
            .filter_map(|h| {
                let s = *spans_ci.get(h.start as usize - 1)?;
                let e = *spans_ci.get(h.end as usize - 1)?;
                Some((s.0, e.1))
            })
            .collect();
        ranges.sort_unstable();
        ranges.dedup();
        wrap_spans(&text, &ranges, pre, post)
    }

    /// snippet(): a `n`-token window around the densest hit cluster,
    /// hits wrapped, ellipses at cut edges.
    fn snippet(&self, rowid: i64, col: i64, pre: &str, post: &str, ell: &str, n: usize) -> String {
        let Some((_, spans, _)) = self.index.docs.get(&rowid) else {
            return String::new();
        };
        // Choose the column: explicit, or the one with the most hits.
        let ci = if col >= 0 && (col as usize) < spans.len() {
            col as usize
        } else {
            let mut best = 0usize;
            let mut best_hits = 0usize;
            for c in 0..spans.len() {
                let h = self.active_hits_in(rowid, c).len();
                if h > best_hits {
                    best_hits = h;
                    best = c;
                }
            }
            best
        };
        let text = self.doc_text(rowid, ci);
        let spans_ci = &spans[ci];
        let n_tokens = spans_ci.len();
        let hits = self.active_hits_in(rowid, ci);
        if n_tokens == 0 {
            return String::new();
        }
        // Window: n tokens containing the most hit starts; prefer the
        // earliest such window (SQLite scans for the densest region).
        let n = n.max(1).min(n_tokens);
        let mut best_start = 0usize;
        let mut best_score = usize::MAX;
        for start in 0..=(n_tokens - n) {
            let end = start + n; // exclusive token index
            let score = hits
                .iter()
                .filter(|h| h.start as usize > start && h.start as usize <= end)
                .count();
            if score > 0 && score < best_score {
                // More hits than the previous best → but "best" = MOST
                // hits; track max. (SQLite: first window with the max.)
                best_score = usize::MAX - score;
                best_start = start;
                if score == hits.len() {
                    break;
                }
            }
        }
        if hits.is_empty() {
            best_start = 0;
        }
        let end_tok = (best_start + n).min(n_tokens);
        // Byte range of the window.
        let b_start = spans_ci[best_start].0;
        let b_end = spans_ci[end_tok - 1].1;
        let window = &text[b_start as usize..b_end as usize];
        // Hit ranges inside the window.
        let ranges: Vec<(u32, u32)> = hits
            .iter()
            .filter_map(|h| {
                let s = *spans_ci.get(h.start as usize - 1)?;
                let e = *spans_ci.get(h.end as usize - 1)?;
                if s.0 >= b_start && e.1 <= b_end {
                    Some((s.0 - b_start, e.1 - b_start))
                } else {
                    None
                }
            })
            .collect();
        let mut out = String::new();
        if best_start > 0 {
            out.push_str(ell);
        }
        out.push_str(&wrap_spans(window, &ranges, pre, post));
        if end_tok < n_tokens {
            out.push_str(ell);
        }
        out
    }

    fn active_hits_in(&self, rowid: i64, col: usize) -> Vec<PhraseHit> {
        let Some(q) = &self.active_query else {
            return Vec::new();
        };
        let phrases = collect_phrases(q);
        let mut out = Vec::new();
        for p in phrases {
            if let Some(rows) = self.phrase_rows(p).get(&rowid) {
                out.extend(rows.iter().filter(|h| h.col == col).cloned());
            }
        }
        out.sort_by_key(|h| h.start);
        out
    }

    fn doc_text(&self, rowid: i64, col: usize) -> String {
        self.index
            .docs
            .get(&rowid)
            .map(|(vals, _, _)| value_text(vals.get(col)))
            .unwrap_or_default()
    }
}

/// One phrase occurrence in a row.
#[derive(Clone, Debug)]
struct PhraseHit {
    col: usize,
    start: u16,
    end: u16,
}

/// Re-tokenize every phrase's terms with the table's tokenizer —
/// SQLite tokenizes the query string with the table's tokenizer, so a
/// trigram table's 5-char query word becomes a 3-gram phrase, and a
/// unicode61 query word with interior separators splits into consecutive
/// terms.
fn retokenize_query(cfg: &FtsConfig, q: Query) -> Query {
    match q {
        Query::Phrase(p) => Query::Phrase(retok_phrase(cfg, p)),
        Query::And(l, r) => Query::And(
            Box::new(retokenize_query(cfg, *l)),
            Box::new(retokenize_query(cfg, *r)),
        ),
        Query::Or(l, r) => Query::Or(
            Box::new(retokenize_query(cfg, *l)),
            Box::new(retokenize_query(cfg, *r)),
        ),
        Query::Not(l, r) => Query::Not(
            Box::new(retokenize_query(cfg, *l)),
            Box::new(retokenize_query(cfg, *r)),
        ),
        Query::Near(ps, d) => {
            Query::Near(ps.into_iter().map(|p| retok_phrase(cfg, p)).collect(), d)
        }
    }
}

fn retok_phrase(cfg: &FtsConfig, p: Phrase) -> Phrase {
    let mut terms: Vec<QueryTerm> = Vec::new();
    for t in &p.terms {
        let toks = tokenize_query_terms(&cfg.tokenizer, &t.text);
        if toks.is_empty() {
            // Separator-only / empty words keep their literal form (they
            // simply never match — same as SQLite).
            terms.push(t.clone());
            continue;
        }
        for (i, tk) in toks.iter().enumerate() {
            terms.push(QueryTerm {
                text: tk.clone(),
                prefix: false,
                initial: t.initial && i == 0,
            });
        }
        // The prefix flag rides the LAST token of the expansion.
        if t.prefix {
            if let Some(last) = terms.last_mut() {
                last.prefix = true;
            }
        }
    }
    Phrase {
        terms,
        cols: p.cols,
    }
}

/// Query-side tokenization by tokenizer type: every token the table's
/// tokenizer would extract from the term text (trigram → 3-grams,
/// porter → stemmed).
fn tokenize_query_terms(tok: &Tokenizer, text: &str) -> Vec<String> {
    match tok {
        Tokenizer::Trigram => {
            let folded: String = text.chars().flat_map(|c| c.to_lowercase()).collect();
            let chars: Vec<char> = folded.chars().collect();
            if chars.len() < 3 {
                return Vec::new();
            }
            chars
                .windows(3)
                .map(|w| w.iter().collect::<String>())
                .collect()
        }
        base => {
            let toks = tokenize_base(base, text);
            if let Tokenizer::Porter(_) = base {
                toks.iter().map(|t| porter_stem(&t.text)).collect()
            } else {
                toks.iter().map(|t| t.text.clone()).collect()
            }
        }
    }
}

/// Every phrase in the query tree (bm25 sums over them).
fn collect_phrases(q: &Query) -> Vec<&Phrase> {
    fn go<'q>(q: &'q Query, out: &mut Vec<&'q Phrase>) {
        match q {
            Query::Phrase(p) => out.push(p),
            Query::And(l, r) | Query::Or(l, r) | Query::Not(l, r) => {
                go(l, out);
                go(r, out);
            }
            Query::Near(ps, _) => {
                for p in ps {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    go(q, &mut v);
    v
}

/// Wrap the given (sorted, non-overlapping) byte ranges.
fn wrap_spans(text: &str, ranges: &[(u32, u32)], pre: &str, post: &str) -> String {
    let mut out = String::with_capacity(text.len() + ranges.len() * (pre.len() + post.len()));
    let mut cur = 0u32;
    for (s, e) in ranges {
        if *s < cur {
            continue; // overlapping
        }
        out.push_str(&text[cur as usize..*s as usize]);
        out.push_str(pre);
        out.push_str(&text[*s as usize..*e as usize]);
        out.push_str(post);
        cur = *e;
    }
    out.push_str(&text[cur as usize..]);
    out
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(i) => Some(*i as f64),
        Value::Real(f) => Some(*f),
        Value::Text(t) => t.parse().ok(),
        _ => None,
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        Value::Real(f) => *f as i64,
        Value::Text(t) => t.parse().unwrap_or(0),
        _ => 0,
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(t) => t.as_str().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format!("{f}"),
        _ => String::new(),
    }
}

// ============================================================================
// The virtual table + cursor + module
// ============================================================================

struct Fts5Table {
    state: Arc<parking_lot::Mutex<FtsState>>,
}

impl Fts5Table {
    fn new(cfg: FtsConfig) -> Self {
        let n = cfg.columns.len();
        Fts5Table {
            state: Arc::new(parking_lot::Mutex::new(FtsState {
                cfg,
                index: FtsIndex::new(),
                active_query: None,
                weights: vec![1.0; n],
            })),
        }
    }
}

impl VirtualTable for Fts5Table {
    fn columns(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .cfg
            .columns
            .iter()
            .map(|c| (c.name.clone(), String::new()))
            .collect()
    }

    fn aux_columns(&self) -> Vec<(String, String)> {
        let cfg = &self.state.lock().cfg;
        vec![
            (cfg.table.clone(), String::new()),
            ("rank".to_string(), String::new()),
        ]
    }

    fn shadow_tables(&self) -> Vec<ShadowTable> {
        let st = self.state.lock();
        vec![ShadowTable {
            name: format!("{}_content", st.cfg.table),
            create_sql: format!(
                "CREATE TABLE \"{}_content\"({})",
                st.cfg.table,
                st.cfg
                    .columns
                    .iter()
                    .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            content: true,
            content_map: None,
        }]
    }

    fn external_content(&self) -> Option<(String, String)> {
        let st = self.state.lock();
        let tbl = st.cfg.external()?.to_string();
        let rowid_col = st
            .cfg
            .content_rowid
            .clone()
            .unwrap_or_else(|| "rowid".to_string());
        Some((tbl, rowid_col))
    }

    fn aux_functions(&self) -> &'static [&'static str] {
        &["bm25", "highlight", "snippet"]
    }

    fn best_index(&self, constraints: &[VtabConstraint]) -> Result<IndexInfo> {
        let st = self.state.lock();
        let mut info = IndexInfo::full_scan(constraints.len());
        let mut strategy: Option<String> = None;
        let mut weights: Option<String> = None;
        let mut like_pat: Option<String> = None;
        let mut rowid_eq = false;
        let n_user = st.n_user_cols();
        for (i, c) in constraints.iter().enumerate() {
            match c.op {
                VtabConstraintOp::Match => match c.column {
                    Some(col) if col == n_user + 1 => {
                        // rank MATCH 'bm25(...)'
                        if let crate::sql::ast::Expr::Literal(crate::types::Value::Text(w)) =
                            &c.expr
                        {
                            weights = Some(w.as_str().to_string());
                            info.handled[i] = true;
                        }
                    }
                    Some(col) if col <= n_user => {
                        // Self column or a user column. EVERY MATCH
                        // constraint is handled — multiple MATCHs AND
                        // (SQLite: `t MATCH 'a' AND t MATCH 'b'`).
                        match &c.expr {
                            crate::sql::ast::Expr::Literal(crate::types::Value::Text(q)) => {
                                let q = q.as_str().to_string();
                                strategy = Some(match strategy.take() {
                                    Some(prev) if !prev.is_empty() => {
                                        format!("{prev} AND {q}")
                                    }
                                    _ => q,
                                });
                                info.handled[i] = true;
                                info.estimated_cost = 10.0;
                                info.estimated_rows = 10;
                            }
                            crate::sql::ast::Expr::Parameter(_) => {
                                // Bound at filter time via filter_args.
                                strategy = Some(match strategy.take() {
                                    Some(prev) if !prev.is_empty() => prev,
                                    _ => String::new(),
                                });
                                info.handled[i] = true;
                                info.estimated_cost = 10.0;
                                info.estimated_rows = 10;
                            }
                            _ => {
                                return Err(Error::semantic(
                                    "unable to use function MATCH in the requested context",
                                ))
                            }
                        }
                    }
                    _ => {}
                },
                VtabConstraintOp::Like => {
                    if st.cfg.tokenizer == Tokenizer::Trigram {
                        if let crate::sql::ast::Expr::Literal(crate::types::Value::Text(pat)) =
                            &c.expr
                        {
                            if pat.starts_with('%') && pat.ends_with('%') && pat.len() >= 5 {
                                like_pat = Some(pat[1..pat.len() - 1].to_string());
                                info.handled[i] = true;
                                info.estimated_cost = 50.0;
                            }
                        }
                    }
                }
                VtabConstraintOp::Eq if c.column.is_none() => {
                    rowid_eq = true;
                    info.handled[i] = true;
                    info.estimated_cost = 5.0;
                }
                _ => {}
            }
        }
        // Compose the idx_str: "match:<query>\u{1}weights:<w>"
        let mut parts: Vec<String> = Vec::new();
        if let Some(q) = strategy {
            info.idx_num = 1;
            parts.push(format!("match:{q}"));
        } else if let Some(pat) = like_pat {
            info.idx_num = 3;
            let q: String = pat.chars().flat_map(|c| c.to_lowercase()).collect();
            parts.push(format!("match:{q}"));
        } else if rowid_eq {
            info.idx_num = 4;
        } else {
            info.estimated_cost = (st.index.n_rows() as f64) * 10.0 + 1000.0;
            info.estimated_rows = st.index.n_rows();
        }
        if let Some(w) = weights {
            parts.push(format!("weights:{w}"));
        }
        if !parts.is_empty() {
            info.idx_str = Some(parts.join("\u{1}"));
        }
        Ok(info)
    }

    fn open(&self) -> Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(Fts5Cursor {
            state: self.state.clone(),
            rows: Vec::new(),
            pos: 0,
            weights: None,
        }))
    }

    fn update(&mut self, ops: Vec<UpdateOp>) -> Result<Vec<Option<i64>>> {
        let mut st = self.state.lock();
        let n_user = st.n_user_cols();
        let cfg = st.cfg.clone();
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match (op.old_rowid, op.new_rowid, op.columns.len()) {
                (None, rid, _) => {
                    let mut vals: Vec<Value> = Vec::with_capacity(n_user);
                    for i in 0..n_user {
                        vals.push(op.columns.get(i).cloned().flatten().unwrap_or(Value::Null));
                    }
                    let rid = rid.unwrap_or_else(|| {
                        st.index.docs.keys().next_back().copied().unwrap_or(0) + 1
                    });
                    st.index.index_row(&cfg, rid, &vals);
                    out.push(Some(rid));
                }
                (Some(old), Some(new), n) if n > 0 => {
                    let old_vals = st
                        .index
                        .docs
                        .get(&old)
                        .map(|(v, _, _)| v.clone())
                        .unwrap_or_else(|| vec![Value::Null; n_user]);
                    let mut vals: Vec<Value> = Vec::with_capacity(n_user);
                    for i in 0..n_user {
                        vals.push(
                            op.columns
                                .get(i)
                                .cloned()
                                .flatten()
                                .or_else(|| old_vals.get(i).cloned())
                                .unwrap_or(Value::Null),
                        );
                    }
                    st.index.unindex_row(&cfg, old);
                    st.index.index_row(&cfg, new, &vals);
                    out.push(Some(new));
                }
                (Some(old), _, _) => {
                    st.index.unindex_row(&cfg, old);
                    out.push(None);
                }
            }
        }
        Ok(out)
    }

    fn reindex(&mut self, rows: &[(i64, Vec<Value>)]) -> Result<()> {
        let mut st = self.state.lock();
        let cfg = st.cfg.clone();
        let mut index = FtsIndex::new();
        for (rid, vals) in rows {
            index.index_row(&cfg, *rid, vals);
        }
        st.index = index;
        Ok(())
    }

    fn set_rank_weights(&mut self, weights: &[f64]) -> Result<()> {
        let mut st = self.state.lock();
        for (i, w) in weights.iter().enumerate() {
            if i < st.weights.len() {
                st.weights[i] = *w;
            }
        }
        Ok(())
    }

    fn eval_aux(&self, name: &str, rowid: i64, args: &[Value]) -> Result<Value> {
        let st = self.state.lock();
        match name {
            "bm25" => {
                let mut weights = st.weights.clone();
                for (i, a) in args.iter().enumerate() {
                    if let Some(w) = as_f64(a) {
                        if i < weights.len() {
                            weights[i] = w;
                        }
                    }
                }
                Ok(Value::Real(st.bm25(rowid, &weights)))
            }
            "highlight" => match (args.first(), args.get(1), args.get(2)) {
                (Some(c), Some(p), Some(q)) => Ok(Value::Text(
                    st.highlight(rowid, as_i64(c), &text_of(p), &text_of(q))
                        .into(),
                )),
                _ => Err(Error::semantic("highlight: wrong arguments")),
            },
            "snippet" => match (
                args.first(),
                args.get(1),
                args.get(2),
                args.get(3),
                args.get(4),
            ) {
                (Some(c), Some(p), Some(q), Some(e), Some(n)) => Ok(Value::Text(
                    st.snippet(
                        rowid,
                        as_i64(c),
                        &text_of(p),
                        &text_of(q),
                        &text_of(e),
                        as_i64(n).max(1) as usize,
                    )
                    .into(),
                )),
                _ => Err(Error::semantic("snippet: wrong arguments")),
            },
            _ => Err(Error::semantic(format!("no such aux function: {name}"))),
        }
    }
}

struct Fts5Cursor {
    state: Arc<parking_lot::Mutex<FtsState>>,
    rows: Vec<i64>,
    pos: usize,
    /// Weights captured at filter time (rank MATCH).
    weights: Option<Vec<f64>>,
}

impl VirtualTableCursor for Fts5Cursor {
    fn filter(&mut self, idx_num: usize, idx_str: Option<&str>, args: &[Value]) -> Result<()> {
        self.pos = 0;
        self.rows.clear();
        let mut st = self.state.lock();
        st.active_query = None;
        self.weights = None;
        if idx_num == 0 && idx_str.is_none() {
            // Full scan: every row in rowid order.
            self.rows = st.index.docs.keys().copied().collect();
            return Ok(());
        }
        // Parse the composed strategy.
        let mut query: Option<String> = None;
        let mut weights: Option<Vec<f64>> = None;
        if let Some(s) = idx_str {
            for part in s.split('\u{1}') {
                if let Some(q) = part.strip_prefix("match:") {
                    query = Some(q.to_string());
                } else if let Some(w) = part.strip_prefix("weights:") {
                    // 'bm25(w0, w1, ...)' — also accept bare numbers.
                    let inner = w
                        .trim()
                        .trim_start_matches("bm25(")
                        .trim_end_matches(')')
                        .trim();
                    weights = Some(
                        inner
                            .split(',')
                            .filter_map(|x| x.trim().parse::<f64>().ok())
                            .collect(),
                    );
                }
            }
        }
        if let Some(w) = weights {
            for (i, ww) in w.iter().enumerate() {
                if i < st.weights.len() {
                    st.weights[i] = *ww;
                }
            }
            self.weights = Some(st.weights.clone());
        }
        match idx_num {
            1 => {
                // MATCH: the query may have been a parameter (bound via
                // filter_args) or a literal (idx_str).
                let q = match query.as_deref() {
                    Some(q) if !q.is_empty() => q.to_string(),
                    _ => match args.first() {
                        Some(Value::Text(t)) => t.as_str().to_string(),
                        _ => String::new(),
                    },
                };
                if q.is_empty() {
                    self.rows = st.index.docs.keys().copied().collect();
                    return Ok(());
                }
                let parsed = retokenize_query(&st.cfg, parse_query(&st.cfg, &q)?);
                let rows = st.eval_query(&parsed);
                st.active_query = Some(parsed);
                self.rows = rows.keys().copied().collect();
                Ok(())
            }
            3 => {
                // trigram LIKE '%pat%': the pattern as a query.
                let q = match query.as_deref() {
                    Some(q) => q.to_string(),
                    None => match args.first() {
                        Some(Value::Text(t)) => t.as_str().to_string(),
                        _ => String::new(),
                    },
                };
                if q.is_empty() {
                    self.rows = st.index.docs.keys().copied().collect();
                    return Ok(());
                }
                let parsed = retokenize_query(&st.cfg, parse_query(&st.cfg, &q)?);
                let rows = st.eval_query(&parsed);
                st.active_query = Some(parsed);
                self.rows = rows.keys().copied().collect();
                Ok(())
            }
            4 => {
                // rowid = ? — the bound value.
                match args.first().map(as_i64) {
                    Some(r) if st.index.docs.contains_key(&r) => self.rows = vec![r],
                    _ => self.rows = Vec::new(),
                }
                Ok(())
            }
            _ => {
                self.rows = st.index.docs.keys().copied().collect();
                Ok(())
            }
        }
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }

    fn column(&self, i: usize) -> Result<Value> {
        let st = self.state.lock();
        let rowid = match self.rows.get(self.pos) {
            Some(r) => *r,
            None => return Ok(Value::Null),
        };
        let n_user = st.n_user_cols();
        if i < n_user {
            if st.cfg.contentless() {
                return Ok(Value::Null);
            }
            return Ok(st
                .index
                .docs
                .get(&rowid)
                .and_then(|(vals, _, _)| vals.get(i).cloned())
                .unwrap_or(Value::Null));
        }
        if i == n_user {
            // Table self column: the rowid.
            return Ok(Value::Integer(rowid));
        }
        if i == n_user + 1 {
            // rank = −bm25 under the active query.
            let weights = self.weights.clone().unwrap_or_else(|| st.weights.clone());
            return Ok(Value::Real(st.bm25(rowid, &weights)));
        }
        Ok(Value::Null)
    }

    fn rowid(&self) -> Result<i64> {
        Ok(*self.rows.get(self.pos).unwrap_or(&0))
    }
}

/// The module: `CREATE VIRTUAL TABLE ... USING fts5(...)`.
pub struct Fts5Module;

impl VirtualTableModule for Fts5Module {
    fn name(&self) -> &str {
        "fts5"
    }

    fn caps(&self) -> u32 {
        ModuleCaps::WRITABLE
    }

    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        let cfg = FtsConfig::parse(table, args)?;
        Ok(Box::new(Fts5Table::new(cfg)))
    }

    fn connect(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        self.create(table, args)
    }

    fn destroy(&self, table: &str, _args: &[String]) -> Result<()> {
        let _ = table;
        Ok(())
    }
}

/// The module instance (registered at Database construction).
pub fn module() -> std::sync::Arc<dyn VirtualTableModule> {
    std::sync::Arc::new(Fts5Module)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FtsConfig {
        FtsConfig::parse("t", &["a".to_string(), "b".to_string()]).unwrap()
    }

    #[test]
    fn query_lexer_tokens() {
        let mut lx = QueryLexer::new("NEAR(beta delta, 2)");
        let mut out = Vec::new();
        while let Some(t) = lx.next().unwrap() {
            out.push(t);
        }
        assert_eq!(
            out,
            vec![
                QTok::Word("NEAR".into(), false, false),
                QTok::LParen,
                QTok::Word("beta".into(), false, false),
                QTok::Word("delta".into(), false, false),
                QTok::Comma,
                QTok::Word("2".into(), false, false),
                QTok::RParen,
            ]
        );
    }

    #[test]
    fn query_parse_near() {
        let c = cfg();
        let q = parse_query(&c, "NEAR(beta delta, 2)").unwrap();
        match q {
            Query::Near(phrases, d) => {
                assert_eq!(d, 2);
                assert_eq!(phrases.len(), 2, "NEAR words are SEPARATE phrases");
            }
            other => panic!("expected Near, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;
    #[test]
    fn query_parse_prefix() {
        let c = FtsConfig::parse("t", &["a".to_string()]).unwrap();
        match parse_query(&c, "quic*") {
            Ok(Query::Phrase(p)) => {
                assert_eq!(p.terms.len(), 1);
                assert!(p.terms[0].prefix, "prefix flag: {:?}", p.terms);
                assert_eq!(p.terms[0].text, "quic");
            }
            other => panic!("bad parse: {other:?}"),
        }
    }
}
