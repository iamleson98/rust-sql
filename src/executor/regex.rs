//! POSIX ERE regular-expression engine (PostgreSQL / SQLite REGEXP surface).
//!
//! The engine is dependency-free, like the rest of the executor: a
//! recursive-descent parser for POSIX Extended Regular Expressions, a
//! Thompson-NFA compiler, and a Pike-style VM simulation. Matching is
//! **leftmost-longest** (POSIX semantics — first the earliest start, then
//! the longest match from that start), not Perl's leftmost-first.
//!
//! Supported syntax (documented divergences from strict POSIX in brackets):
//! - literals, `.` (any character incl. newline), `|` alternation
//! - grouping `(...)` with captures (group 0 = whole match)
//! - quantifiers `*` `+` `?` `{n}` `{n,}` `{n,m}` (greedy; POSIX has no
//!   lazy quantifiers — and greed is irrelevant under longest-match)
//! - bracket expressions `[abc]` `[^a-z]` with ranges and the POSIX
//!   classes `[[:alpha:]]` `[[:digit:]]` `[[:alnum:]]` `[[:space:]]`
//!   `[[:upper:]]` `[[:lower:]]` `[[:punct:]]` `[[:xdigit:]]` `[[:blank:]]`
//!   `[[:cntrl:]]` `[[:print:]]` `[[:graph:]]`
//! - anchors `^` `$` (whole-string; no REG_NEWLINE mode)
//! - escapes: outside brackets `\` quotes the next character, plus the
//!   C-style `\n \t \r \f \v \a` and the PG-ARE classes `\d \D \w \W \s \S`
//!   [strict POSIX has NO escapes at all — this is the pragmatic superset
//!   every real engine (incl. PostgreSQL's ARE) accepts]
//! - inside brackets, `\` is a LITERAL backslash (strict POSIX rule): use
//!   `[]a]` `[-z]` `[a-]` placements instead of escapes
//!
//! Not supported (documented, same as POSIX ERE): backreferences,
//! lookahead/lookbehind, lazy quantifiers, inline `(?...)` modes.
//!
//! Linear-time guarantee: the NFA simulation is O(len × prog) per start
//! position and O(len) in memory — no exponential backtracking (the
//! reason PostgreSQL itself moved off backtracking engines).
//!
//! Function surface (dispatched from `expr.rs` after user functions):
//! - `regexp(pattern, string)` — SQLite's hook convention (pattern FIRST)
//! - `regexp_like(source, pattern [, flags])` — PostgreSQL 15
//! - `regexp_replace(source, pattern, replacement [, flags])` — PG; `\1`..
//!   `\9` group refs, `\&` whole match, `\\` backslash; flags `i` `c` `g`
//! - `regexp_substr(source, pattern [, start [, occurrence]])` — PG 15
//!   (simplified arg list, documented)
//! - `regexp_instr(source, pattern [, start [, occurrence]])` — PG 15
//!   (simplified arg list, documented)
//! - `regexp_count(source, pattern [, start])` — PG 15
//!
//! NULL-in-NULL-out for every argument (PostgreSQL semantics).
//! Character positions are 1-based (PostgreSQL), not byte-based.

use crate::error::{Error, Result};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;

// ============================================================================
// Character classes
// ============================================================================

/// A set of inclusive character ranges. Negation is compiled away by
/// complementing against the full `char` domain at parse time, so the
/// matcher is a pure range scan.
#[derive(Clone, Debug, Default)]
struct Class {
    ranges: Vec<(char, char)>,
}

impl Class {
    fn single(c: char) -> Self {
        Class {
            ranges: vec![(c, c)],
        }
    }

    /// Complement against the full char domain.
    fn complement(&self) -> Class {
        let mut sorted = self.ranges.clone();
        sorted.sort();
        let mut out: Vec<(char, char)> = Vec::with_capacity(sorted.len() + 1);
        let mut next = 0u32; // first code point not yet covered
        for (lo, hi) in sorted {
            let lo = lo as u32;
            let hi = hi as u32;
            if lo > next {
                if let (Some(a), Some(b)) =
                    (char::from_u32(next), char::from_u32(lo.saturating_sub(1)))
                {
                    out.push((a, b));
                }
            }
            next = next.max(hi.saturating_add(1));
        }
        if next <= char::MAX as u32 {
            if let Some(a) = char::from_u32(next) {
                out.push((a, char::MAX));
            }
        }
        Class { ranges: out }
    }

    fn union(mut self, other: &Class) -> Class {
        self.ranges.extend_from_slice(&other.ranges);
        self.normalize()
    }

    /// Sort + merge overlapping/adjacent ranges.
    fn normalize(mut self) -> Class {
        self.ranges.sort();
        let mut merged: Vec<(char, char)> = Vec::with_capacity(self.ranges.len());
        for (lo, hi) in self.ranges {
            if let Some(last) = merged.last_mut() {
                if lo as u32 <= last.1 as u32 + 1 {
                    if hi > last.1 {
                        last.1 = hi;
                    }
                    continue;
                }
            }
            merged.push((lo, hi));
        }
        Class { ranges: merged }
    }

    fn contains(&self, c: char) -> bool {
        self.ranges
            .binary_search_by(|&(lo, hi)| {
                if c < lo {
                    std::cmp::Ordering::Greater
                } else if c > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// Case-insensitive membership: try the char, then its simple case
    /// variants (both directions — sufficient for ASCII letters, which is
    /// the documented folding scope).
    fn contains_fold(&self, c: char) -> bool {
        if self.contains(c) {
            return true;
        }
        let mut lower = c.to_lowercase();
        if let (Some(l), None) = (lower.next(), lower.next()) {
            if self.contains(l) {
                return true;
            }
        }
        let mut upper = c.to_uppercase();
        if let (Some(u), None) = (upper.next(), upper.next()) {
            if self.contains(u) {
                return true;
            }
        }
        false
    }

    fn range(lo: char, hi: char) -> Self {
        Class {
            ranges: vec![(lo, hi)],
        }
    }
}

fn posix_class(name: &str) -> Option<Class> {
    let cls = match name {
        "alpha" => Class::range('a', 'z').union(&Class::range('A', 'Z')),
        "digit" => Class::range('0', '9'),
        "alnum" => {
            Class::range('0', '9').union(&Class::range('a', 'z').union(&Class::range('A', 'Z')))
        }
        "upper" => Class::range('A', 'Z'),
        "lower" => Class::range('a', 'z'),
        "space" => Class::single(' ').union(
            &['\t', '\n', '\u{b}', '\u{c}', '\r']
                .iter()
                .fold(Class::default(), |acc, c| acc.union(&Class::single(*c))),
        ),
        "blank" => Class::single(' ').union(&Class::single('\t')),
        "punct" => Class::range('!', '/').union(
            &Class::range(':', '@')
                .union(&Class::range('[', '`'))
                .union(&Class::range('{', '~')),
        ),
        "xdigit" => Class::range('0', '9')
            .union(&Class::range('a', 'f'))
            .union(&Class::range('A', 'F')),
        "cntrl" => Class::range('\0', '\u{1f}').union(&Class::single('\u{7f}')),
        "print" => Class::range(' ', '~'),
        "graph" => Class::range('!', '~'),
        _ => return None,
    };
    Some(cls)
}

// Perl-style escape classes (PG ARE compatible).
fn perl_class(c: char) -> Option<Class> {
    let base = match c {
        'd' => Class::range('0', '9'),
        'w' => Class::range('0', '9')
            .union(&Class::range('a', 'z'))
            .union(&Class::range('A', 'Z'))
            .union(&Class::single('_')),
        's' => posix_class("space").unwrap(),
        _ => return None,
    };
    Some(base)
}

// ============================================================================
// Parse tree
// ============================================================================

#[derive(Clone, Debug)]
enum Ast {
    Empty,
    Char(char),
    Any,
    Class(Class),
    Concat(Vec<Ast>),
    Alt(Vec<Ast>),
    Star(Box<Ast>),
    Plus(Box<Ast>),
    Quest(Box<Ast>),
    Repeat(Box<Ast>, usize, Option<usize>),
    Group(Box<Ast>, usize),
    AnchorStart,
    AnchorEnd,
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    n_groups: usize,
    _src: &'a str,
}

fn perr(msg: &str) -> Error {
    Error::runtime(format!("invalid regular expression: {}", msg))
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            chars: src.chars().collect(),
            pos: 0,
            n_groups: 0,
            _src: src,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn parse(&mut self) -> Result<Ast> {
        let ast = self.parse_alt()?;
        if self.pos != self.chars.len() {
            return Err(perr("unexpected character after expression"));
        }
        Ok(ast)
    }

    fn parse_alt(&mut self) -> Result<Ast> {
        let mut branches = vec![self.parse_branch()?];
        while self.peek() == Some('|') {
            self.bump();
            branches.push(self.parse_branch()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Ast::Alt(branches))
        }
    }

    fn parse_branch(&mut self) -> Result<Ast> {
        let mut pieces = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            pieces.push(self.parse_piece()?);
        }
        if pieces.is_empty() {
            return Ok(Ast::Empty);
        }
        if pieces.len() == 1 {
            Ok(pieces.pop().unwrap())
        } else {
            Ok(Ast::Concat(pieces))
        }
    }

    fn parse_piece(&mut self) -> Result<Ast> {
        let atom = self.parse_atom()?;
        // A quantifier directly after another quantifier is an error
        // (`a**`): POSIX leaves it undefined; PostgreSQL errors, and so do
        // we — silently re-quantifying changes match semantics.
        match self.peek() {
            Some('*') => {
                self.bump();
                self.reject_double_quant()?;
                Ok(Ast::Star(Box::new(atom)))
            }
            Some('+') => {
                self.bump();
                self.reject_double_quant()?;
                Ok(Ast::Plus(Box::new(atom)))
            }
            Some('?') => {
                self.bump();
                self.reject_double_quant()?;
                Ok(Ast::Quest(Box::new(atom)))
            }
            Some('{') => {
                // Only an INTERVAL (digits, optional `,`) is a quantifier;
                // anything else is a literal brace (POSIX rule).
                if let Some((n, m)) = self.try_parse_interval()? {
                    self.reject_double_quant()?;
                    if let Some(m) = m {
                        if m < n {
                            return Err(perr("invalid repeat interval {n,m} with m < n"));
                        }
                    }
                    return Ok(Ast::Repeat(Box::new(atom), n, m));
                }
                Ok(atom)
            }
            _ => Ok(atom),
        }
    }

    fn reject_double_quant(&self) -> Result<()> {
        match self.peek() {
            Some('*') | Some('+') | Some('?') => {
                Err(perr("repetition operator follows repetition operator"))
            }
            Some('{') => {
                // Only when it actually parses as an interval.
                let mut probe = Parser {
                    chars: self.chars.clone(),
                    pos: self.pos,
                    n_groups: self.n_groups,
                    _src: self._src,
                };
                if probe.try_parse_interval()?.is_some() {
                    Err(perr("repetition operator follows repetition operator"))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    /// Try to consume `{n}` / `{n,}` / `{n,m}`. Returns None (consuming
    /// nothing) when the brace is not a valid interval.
    fn try_parse_interval(&mut self) -> Result<Option<(usize, Option<usize>)>> {
        if self.peek() != Some('{') {
            return Ok(None);
        }
        let mut i = 1usize;
        let mut n_str = String::new();
        while let Some(c) = self.peek_at(i) {
            if c.is_ascii_digit() {
                n_str.push(c);
                i += 1;
            } else {
                break;
            }
        }
        if n_str.is_empty() {
            return Ok(None); // literal '{'
        }
        let n: usize = n_str.parse().map_err(|_| perr("repeat count too large"))?;
        match self.peek_at(i) {
            Some('}') => {
                self.pos += i + 1;
                Ok(Some((n, Some(n))))
            }
            Some(',') => {
                i += 1;
                let mut m_str = String::new();
                while let Some(c) = self.peek_at(i) {
                    if c.is_ascii_digit() {
                        m_str.push(c);
                        i += 1;
                    } else {
                        break;
                    }
                }
                if self.peek_at(i) != Some('}') {
                    return Ok(None); // malformed -> literal brace
                }
                let m = if m_str.is_empty() {
                    None
                } else {
                    Some(m_str.parse().map_err(|_| perr("repeat count too large"))?)
                };
                self.pos += i + 1;
                Ok(Some((n, m)))
            }
            _ => Ok(None), // literal '{'
        }
    }

    fn parse_atom(&mut self) -> Result<Ast> {
        let c = self
            .bump()
            .ok_or_else(|| perr("unexpected end of pattern"))?;
        match c {
            '.' => Ok(Ast::Any),
            '^' => Ok(Ast::AnchorStart),
            '$' => Ok(Ast::AnchorEnd),
            '*' | '+' | '?' => Err(perr("nothing to repeat")),
            ')' => Err(perr("unmatched closing parenthesis")),
            ']' => Err(perr("unmatched closing bracket")),
            '(' => {
                self.n_groups += 1;
                if self.n_groups > 100 {
                    return Err(perr("too many capture groups (max 100)"));
                }
                let gid = self.n_groups;
                let inner = self.parse_alt()?;
                if self.bump() != Some(')') {
                    return Err(perr("missing closing parenthesis"));
                }
                Ok(Ast::Group(Box::new(inner), gid))
            }
            '[' => self.parse_bracket(),
            '\\' => {
                let e = self
                    .bump()
                    .ok_or_else(|| perr("trailing backslash at end of pattern"))?;
                // Perl-style escape classes (PG ARE compatible).
                if let Some(base) = perl_class(e) {
                    return Ok(Ast::Class(base));
                }
                // Complemented forms.
                if e == 'D' || e == 'W' || e == 'S' {
                    let base =
                        perl_class(e.to_ascii_lowercase()).ok_or_else(|| perr("unknown escape"))?;
                    return Ok(Ast::Class(base.complement()));
                }
                // C-style control escapes (PG ARE).
                let ctrl = match e {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'f' => '\u{c}',
                    'v' => '\u{b}',
                    'a' => '\u{7}',
                    _ => return Ok(Ast::Char(e)), // literal quote
                };
                Ok(Ast::Char(ctrl))
            }
            '{' => {
                // A brace that didn't parse as an interval quantifier is a
                // literal (checked by parse_piece before we get here).
                Ok(Ast::Char('{'))
            }
            other => Ok(Ast::Char(other)),
        }
    }

    fn parse_bracket(&mut self) -> Result<Ast> {
        let mut negated = false;
        let mut items = Class::default();
        let mut first = true;
        loop {
            let c = self
                .bump()
                .ok_or_else(|| perr("unterminated bracket expression"))?;
            if first && c == '^' {
                negated = true;
                first = false;
                continue;
            }
            first = false;
            if c == ']' && !items.ranges.is_empty() {
                // POSIX: `[]a]` — a leading `]` is literal; only a
                // non-leading, non-first `]` closes the class.
                return Ok(Ast::Class(if negated {
                    items.complement()
                } else {
                    items.normalize()
                }));
            }
            if c == ']' {
                // First character is `]`: literal.
                items = items.union(&Class::single(']'));
                continue;
            }
            // POSIX class [:alpha:]
            if c == '[' && self.peek() == Some(':') {
                let mut name = String::new();
                let mut i = self.pos + 1; // past ':'
                while let Some(ch) = self.chars.get(i) {
                    if *ch == ':' {
                        break;
                    }
                    name.push(*ch);
                    i += 1;
                }
                if self.chars.get(i) == Some(&':') && self.chars.get(i + 1) == Some(&']') {
                    let cls = posix_class(&name)
                        .ok_or_else(|| perr(&format!("unknown POSIX class [:{}:]", name)))?;
                    items = items.union(&cls);
                    self.pos = i + 2;
                    continue;
                }
                return Err(perr("unterminated POSIX class"));
            }
            // Range: `a-z` (a `-` that is first or last in the class is a
            // literal dash — POSIX rule).
            if self.peek() == Some('-') && self.peek_at(1).is_some() && self.peek_at(1) != Some(']')
            {
                self.bump(); // '-'
                let hi = self
                    .bump()
                    .ok_or_else(|| perr("unterminated bracket expression"))?;
                if (hi as u32) < (c as u32) {
                    return Err(perr("invalid character range (lo > hi)"));
                }
                items = items.union(&Class::range(c, hi));
            } else {
                // NOTE: strict POSIX — a backslash inside brackets is a
                // LITERAL backslash, not an escape.
                items = items.union(&Class::single(c));
            }
        }
    }
}

// ============================================================================
// NFA compilation
// ============================================================================

#[derive(Clone, Debug)]
enum Inst {
    /// Consume one character matching the class.
    Char(Class),
    /// Try `a` first, then `b` (priority — irrelevant under longest-match
    /// but keeps captures deterministic).
    Split(usize, usize),
    Jump(usize),
    /// Record a position into a capture slot.
    Save(usize),
    /// Assert start-of-string.
    AssertStart,
    /// Assert end-of-string.
    AssertEnd,
    Match,
}

/// Compiled program. Immutable after construction; shared through the
/// pattern cache.
pub(crate) struct Prog {
    insts: Vec<Inst>,
    /// Capture groups INCLUDING group 0 (the whole match).
    pub(crate) n_groups: usize,
    /// The whole pattern is a literal string (fast path).
    literal: Option<Vec<char>>,
    /// Every epsilon-reachable first character must be one of these
    /// (None = an empty match is possible, so no start can be skipped).
    first_chars: Option<Class>,
}

const PROG_MAX: usize = 100_000;

struct Compiler {
    insts: Vec<Inst>,
}

impl Compiler {
    fn emit(&mut self, inst: Inst) -> usize {
        self.insts.push(inst);
        self.insts.len() - 1
    }

    fn compile(&mut self, ast: &Ast) -> Result<()> {
        if self.insts.len() > PROG_MAX {
            return Err(perr("regular expression too large"));
        }
        match ast {
            Ast::Empty => {}
            Ast::Char(c) => {
                self.emit(Inst::Char(Class::single(*c)));
            }
            Ast::Any => {
                self.emit(Inst::Char(Class::range('\0', char::MAX)));
            }
            Ast::Class(cl) => {
                self.emit(Inst::Char(cl.clone()));
            }
            Ast::AnchorStart => {
                self.emit(Inst::AssertStart);
            }
            Ast::AnchorEnd => {
                self.emit(Inst::AssertEnd);
            }
            Ast::Concat(parts) => {
                for p in parts {
                    self.compile(p)?;
                }
            }
            Ast::Alt(branches) => {
                // chain of splits with jumps to a common end
                let mut jumps = Vec::new();
                let n = branches.len();
                for (i, b) in branches.iter().enumerate() {
                    if i + 1 < n {
                        let split = self.emit(Inst::Split(0, 0));
                        let alt_start = self.insts.len();
                        self.compile(b)?;
                        jumps.push(self.emit(Inst::Jump(0)));
                        let next = self.insts.len();
                        self.insts[split] = Inst::Split(alt_start, next);
                    } else {
                        self.compile(b)?;
                    }
                }
                let end = self.insts.len();
                for j in jumps {
                    self.insts[j] = Inst::Jump(end);
                }
            }
            Ast::Group(inner, gid) => {
                let slot = 2 * gid;
                self.emit(Inst::Save(slot));
                self.compile(inner)?;
                self.emit(Inst::Save(slot + 1));
            }
            Ast::Star(inner) => {
                // L1: split(L2, L3); L2: inner; jump L1; L3:
                let l1 = self.emit(Inst::Split(0, 0));
                let l2 = self.insts.len();
                self.compile(inner)?;
                self.emit(Inst::Jump(l1));
                let l3 = self.insts.len();
                self.insts[l1] = Inst::Split(l2, l3);
            }
            Ast::Plus(inner) => {
                // L1: inner; split(L1, L2); L2:
                let l1 = self.insts.len();
                self.compile(inner)?;
                let split = self.emit(Inst::Split(0, 0));
                let l2 = self.insts.len();
                self.insts[split] = Inst::Split(l1, l2);
            }
            Ast::Quest(inner) => {
                let split = self.emit(Inst::Split(0, 0));
                let l2 = self.insts.len();
                self.compile(inner)?;
                let l3 = self.insts.len();
                self.insts[split] = Inst::Split(l2, l3);
            }
            Ast::Repeat(inner, n, m) => {
                for _ in 0..*n {
                    self.compile(inner)?;
                }
                match m {
                    None => {
                        self.compile(&Ast::Star(inner.clone()))?;
                    }
                    Some(m) => {
                        // (m - n) nested optionals: a{2,4} = a a (a (a)?)?
                        let extra = m - n;
                        let mut splits = Vec::with_capacity(extra);
                        for _ in 0..extra {
                            let split = self.emit(Inst::Split(0, 0));
                            splits.push(split);
                            self.compile(inner)?;
                        }
                        let end = self.insts.len();
                        // Each optional's failure path jumps past the rest.
                        for (i, split) in splits.iter().enumerate() {
                            // failure target: skip remaining optionals —
                            // jump to the end (nested semantics: skipping an
                            // inner optional means the outer ones are
                            // skipped too, which a plain jump-to-end
                            // reproduces exactly for this shape).
                            let _ = i;
                            self.insts[*split] = Inst::Split(split + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Extract a literal fast-path string (whole pattern literal).
fn literal_of(ast: &Ast) -> Option<Vec<char>> {
    fn walk(a: &Ast, out: &mut Vec<char>) -> bool {
        match a {
            Ast::Char(c) => {
                out.push(*c);
                true
            }
            Ast::Concat(parts) => parts.iter().all(|p| walk(p, out)),
            Ast::Group(inner, _) => walk(inner, out),
            _ => false,
        }
    }
    let mut out = Vec::new();
    if walk(ast, &mut out) {
        Some(out)
    } else {
        None
    }
}

// ============================================================================
// Pike VM (leftmost-longest)
// ============================================================================

/// A successful match: char-index spans for the whole match plus every
/// capture group (`None` = group did not participate).
#[derive(Clone, Debug)]
pub(crate) struct RegexMatch {
    pub start: usize,
    pub end: usize,
    pub groups: Vec<Option<(usize, usize)>>,
}

struct Vm<'p, 't> {
    prog: &'p Prog,
    text: &'t [char],
    ci: bool,
    /// Epoch-stamped visited marks (per pc), so lists never need clearing.
    marks: Vec<u32>,
    epoch: u32,
}

#[derive(Clone)]
struct Thread {
    pc: usize,
    caps: Vec<Option<usize>>,
}

impl<'p, 't> Vm<'p, 't> {
    /// Add a thread with its epsilon closure at position `pos`. Threads
    /// stored in the list always sit at a CONSUMING instruction (Char) or
    /// at Match — epsilon instructions (Jump/Split/Save/assertions) are
    /// resolved during the closure walk.
    fn add_thread(
        &mut self,
        list: &mut Vec<Thread>,
        mut pc: usize,
        caps: Vec<Option<usize>>,
        pos: usize,
    ) {
        loop {
            if pc >= self.prog.insts.len() {
                return;
            }
            if self.marks[pc] == self.epoch {
                return;
            }
            self.marks[pc] = self.epoch;
            match self.prog.insts[pc].clone() {
                Inst::Jump(t) => {
                    pc = t;
                }
                Inst::Split(a, b) => {
                    self.add_thread(list, a, caps.clone(), pos);
                    pc = b;
                }
                Inst::Save(slot) => {
                    let mut c2 = caps.clone();
                    if slot < c2.len() {
                        c2[slot] = Some(pos);
                    }
                    // Continue the closure with the updated caps.
                    pc += 1;
                    self.add_thread(list, pc, c2, pos);
                    return;
                }
                Inst::AssertStart => {
                    if pos == 0 {
                        pc += 1;
                    } else {
                        return;
                    }
                }
                Inst::AssertEnd => {
                    if pos == self.text.len() {
                        pc += 1;
                    } else {
                        return;
                    }
                }
                Inst::Char(_) | Inst::Match => {
                    list.push(Thread { pc, caps });
                    return;
                }
            }
        }
    }

    /// Record a Match thread into `best` (longest wins).
    fn record_match(&self, best: &mut Option<RegexMatch>, t: &Thread, start: usize, end: usize) {
        let is_better = match best {
            None => true,
            Some(b) => end > b.end,
        };
        if !is_better {
            return;
        }
        let groups = t
            .caps
            .chunks(2)
            .enumerate()
            .map(|(gi, pair)| {
                if gi == 0 {
                    Some((start, end))
                } else {
                    match (pair[0], pair[1]) {
                        (Some(a), Some(b)) => Some((a, b)),
                        _ => None,
                    }
                }
            })
            .collect();
        *best = Some(RegexMatch { start, end, groups });
    }

    /// Run the machine anchored at `start`. Returns the LONGEST match
    /// from that start position (POSIX semantics).
    fn run_at(&mut self, start: usize) -> Option<RegexMatch> {
        let n_caps = 2 * self.prog.n_groups;
        let init = vec![None; n_caps];
        let mut clist: Vec<Thread> = Vec::new();
        let mut nlist: Vec<Thread> = Vec::new();
        self.epoch += 1;
        self.add_thread(&mut clist, 0, init, start);
        let mut best: Option<RegexMatch> = None;
        let mut pos = start;
        loop {
            let cur: Vec<Thread> = std::mem::take(&mut clist);
            if pos < self.text.len() {
                let ch = self.text[pos];
                self.epoch += 1;
                for t in cur {
                    match &self.prog.insts[t.pc] {
                        Inst::Char(cl) => {
                            let hit = if self.ci {
                                cl.contains_fold(ch)
                            } else {
                                cl.contains(ch)
                            };
                            if hit {
                                self.add_thread(&mut nlist, t.pc + 1, t.caps, pos + 1);
                            }
                        }
                        // A Match thread reached mid-text records the
                        // [start, pos) span and dies — other threads may
                        // still produce a LONGER match.
                        Inst::Match => {
                            self.record_match(&mut best, &t, start, pos);
                        }
                        _ => {} // unreachable: closure stores only Char/Match
                    }
                }
            } else {
                // End of text: Match threads record; Char threads die.
                self.epoch += 1;
                for t in cur {
                    if let Inst::Match = &self.prog.insts[t.pc] {
                        self.record_match(&mut best, &t, start, pos);
                    }
                }
                return best;
            }
            std::mem::swap(&mut clist, &mut nlist);
            nlist.clear();
            if clist.is_empty() {
                // No live threads: no longer match can appear.
                return best;
            }
            pos += 1;
        }
    }
}

// ============================================================================
// Regex (public API + cache)
// ============================================================================

/// A compiled regular expression.
pub(crate) struct Regex {
    prog: Prog,
    ci: bool,
}

impl Regex {
    pub(crate) fn compile(pattern: &str, ci: bool) -> Result<Regex> {
        let mut parser = Parser::new(pattern);
        let ast = parser.parse()?;
        let n_groups = parser.n_groups + 1; // + group 0
        let literal = literal_of(&ast);
        let mut compiler = Compiler { insts: Vec::new() };
        compiler.compile(&ast)?;
        compiler.emit(Inst::Match);
        let first_chars = first_char_class(&compiler.insts);
        Ok(Regex {
            prog: Prog {
                insts: compiler.insts,
                n_groups,
                literal,
                first_chars,
            },
            ci,
        })
    }

    /// Leftmost-longest search over the whole text.
    pub(crate) fn find(&self, text: &str) -> Option<RegexMatch> {
        let chars: Vec<char> = text.chars().collect();
        self.find_chars(&chars)
    }

    pub(crate) fn find_chars(&self, chars: &[char]) -> Option<RegexMatch> {
        // Literal fast path: substring search (folded when ci).
        if let Some(lit) = &self.prog.literal {
            if self.ci {
                let n = lit.len();
                if n == 0 {
                    return Some(RegexMatch {
                        start: 0,
                        end: 0,
                        groups: vec![Some((0, 0)); self.prog.n_groups],
                    });
                }
                let b_fold: Vec<char> = lit
                    .iter()
                    .map(|c| c.to_lowercase().next().unwrap_or(*c))
                    .collect();
                let b_upper: Vec<char> = lit
                    .iter()
                    .map(|c| c.to_uppercase().next().unwrap_or(*c))
                    .collect();
                if chars.len() >= n {
                    'outer: for s in 0..=(chars.len() - n) {
                        for i in 0..n {
                            let a = chars[s + i];
                            let a_lower = a.to_lowercase().next().unwrap_or(a);
                            let a_upper = a.to_uppercase().next().unwrap_or(a);
                            let b = lit[i];
                            // Case-insensitive literal equality: either
                            // side's simple case variants may agree.
                            if a != b
                                && a_lower != b
                                && a_upper != b
                                && a != b_fold[i]
                                && a != b_upper[i]
                            {
                                continue 'outer;
                            }
                        }
                        return Some(RegexMatch {
                            start: s,
                            end: s + n,
                            groups: vec![Some((s, s + n)); self.prog.n_groups],
                        });
                    }
                }
                return None;
            }
            if lit.is_empty() {
                return Some(RegexMatch {
                    start: 0,
                    end: 0,
                    groups: vec![Some((0, 0)); self.prog.n_groups],
                });
            }
            let n = lit.len();
            if chars.len() >= n {
                for s in 0..=(chars.len() - n) {
                    if chars[s..s + n] == lit[..] {
                        return Some(RegexMatch {
                            start: s,
                            end: s + n,
                            groups: vec![Some((s, s + n)); self.prog.n_groups],
                        });
                    }
                }
            }
            return None;
        }
        let anchored = starts_anchored(&self.prog.insts);
        let mut vm = Vm {
            prog: &self.prog,
            text: chars,
            ci: self.ci,
            marks: vec![0; self.prog.insts.len()],
            epoch: 0,
        };
        let mut s = 0usize;
        while s <= chars.len() {
            // First-character prefilter: skip starts that can't begin a
            // match (empty-match-capable patterns have first_chars = None).
            if let Some(fc) = &self.prog.first_chars {
                if s < chars.len() {
                    let ch = chars[s];
                    let ok = if self.ci {
                        fc.contains_fold(ch)
                    } else {
                        fc.contains(ch)
                    };
                    if !ok {
                        s += 1;
                        continue;
                    }
                }
            }
            if let Some(m) = vm.run_at(s) {
                return Some(m);
            }
            if anchored {
                return None;
            }
            s += 1;
        }
        None
    }
}

/// The set of characters that can begin a match, computed from the
/// epsilon closure of pc 0. `None` = an empty match is possible (no
/// start position can be skipped).
fn first_char_class(insts: &[Inst]) -> Option<Class> {
    let mut seen = vec![false; insts.len()];
    let mut stack = vec![0usize];
    let mut cls = Class::default();
    let mut empty_ok = false;
    while let Some(pc) = stack.pop() {
        if pc >= insts.len() || seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &insts[pc] {
            Inst::Char(c) => cls = cls.union(c),
            Inst::Match => empty_ok = true,
            Inst::Jump(t) => stack.push(*t),
            Inst::Split(a, b) => {
                stack.push(*a);
                stack.push(*b);
            }
            Inst::AssertStart | Inst::AssertEnd => stack.push(pc + 1),
            Inst::Save(_) => stack.push(pc + 1),
        }
    }
    if empty_ok {
        None
    } else {
        Some(cls)
    }
}

fn starts_anchored(insts: &[Inst]) -> bool {
    let mut seen = vec![false; insts.len()];
    let mut stack = vec![0usize];
    while let Some(pc) = stack.pop() {
        if pc >= insts.len() || seen[pc] {
            continue;
        }
        seen[pc] = true;
        match &insts[pc] {
            Inst::AssertStart => return true,
            Inst::Jump(t) => stack.push(*t),
            Inst::Split(a, b) => {
                stack.push(*a);
                stack.push(*b);
            }
            Inst::Char(_) | Inst::Match | Inst::AssertEnd => {}
            Inst::Save(_) => stack.push(pc + 1),
        }
    }
    false
}

// ============================================================================
// Pattern cache
// ============================================================================

thread_local! {
    /// Compiled-pattern cache (thread-local: patterns are per-thread
    /// hot-path values; cap-and-clear keeps memory bounded).
    static REGEX_CACHE: RefCell<HashMap<(String, bool), std::rc::Rc<Regex>>> =
        RefCell::new(HashMap::new());
}

const CACHE_CAP: usize = 128;

/// Compile (or fetch from cache) a pattern. Public within the crate so the
/// `REGEXP` operator arm in expr.rs shares the cache with the function
/// family.
pub(crate) fn cached_compile(pattern: &str, ci: bool) -> Result<std::rc::Rc<Regex>> {
    REGEX_CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if let Some(r) = map.get(&(pattern.to_string(), ci)) {
            return Ok(r.clone());
        }
        let compiled = std::rc::Rc::new(Regex::compile(pattern, ci)?);
        if map.len() >= CACHE_CAP {
            map.clear();
        }
        map.insert((pattern.to_string(), ci), compiled.clone());
        Ok(compiled)
    })
}

// ============================================================================
// SQL function surface
// ============================================================================

/// POSIX flags string (PostgreSQL): `i` = case-insensitive, `c` =
/// case-sensitive, `g` = global replace. Everything else errors (honest
/// unsupported rather than silent ignore).
fn parse_flags(flags: &Value, allow_global: bool) -> Result<(bool, bool)> {
    if flags.is_null() {
        return Ok((false, false));
    }
    let f = flags.as_text();
    let mut ci = false;
    let mut global = false;
    for c in f.chars() {
        match c {
            'i' => ci = true,
            'c' => ci = false,
            'g' if allow_global => global = true,
            'g' => {
                return Err(Error::runtime(
                    "invalid regular expression flag: 'g' (only valid for regexp_replace)",
                ));
            }
            other => {
                return Err(Error::runtime(format!(
                    "invalid regular expression flag: '{}'",
                    other
                )));
            }
        }
    }
    Ok((ci, global))
}

/// 1-based char position -> 0-based char index (clamped like PG: start=0
/// behaves as 1, negatives error).
fn char_start_arg(v: &Value, fname: &str) -> Result<usize> {
    let n = match v {
        Value::Integer(i) if *i >= 0 => *i as usize,
        Value::Integer(i) => {
            return Err(Error::runtime(format!(
                "{}: start position must be >= 1, got {}",
                fname, i
            )));
        }
        Value::Real(f) if *f >= 0.0 && f.fract() == 0.0 => *f as usize,
        _ => {
            return Err(Error::runtime(format!(
                "{}: start position must be an integer",
                fname
            )));
        }
    };
    Ok(n.saturating_sub(1))
}

fn occurrence_arg(v: &Value, fname: &str) -> Result<usize> {
    match v {
        Value::Integer(i) if *i >= 1 => Ok(*i as usize),
        Value::Integer(i) => Err(Error::runtime(format!(
            "{}: occurrence must be >= 1, got {}",
            fname, i
        ))),
        Value::Null => Ok(1),
        _ => Err(Error::runtime(format!(
            "{}: occurrence must be an integer",
            fname
        ))),
    }
}

/// Slice chars [from, to) back into a String.
fn chars_slice(chars: &[char], from: usize, to: usize) -> String {
    chars[from.min(chars.len())..to.min(chars.len())]
        .iter()
        .collect()
}

/// Leftmost-longest search from a 0-based char offset. (Reuses the
/// whole-text matcher on the tail and shifts the spans back — simple and
/// correct; the tail allocation is bounded by the subject length.)
fn find_from(re: &Regex, chars: &[char], pos: usize) -> Option<RegexMatch> {
    let start = pos.min(chars.len());
    let tail: String = chars[start..].iter().collect();
    re.find(&tail).map(|m| RegexMatch {
        start: m.start + start,
        end: m.end + start,
        groups: m.groups,
    })
}

/// Expand a replacement template: `\1`..`\9` group refs, `\&` whole match,
/// `\\` literal backslash, any other `\x` passes through both characters.
fn expand_replacement(repl: &[char], m: &RegexMatch, chars: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < repl.len() {
        let c = repl[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        match repl.get(i + 1) {
            None => {
                out.push('\\');
                i += 1;
            }
            Some(&n) if n.is_ascii_digit() && n != '0' => {
                let gid = (n as u8 - b'1') as usize + 1;
                if let Some(Some((a, b))) = m.groups.get(gid) {
                    out.push_str(&chars_slice(chars, *a, *b));
                }
                i += 2;
            }
            Some('&') => {
                out.push_str(&chars_slice(chars, m.start, m.end));
                i += 2;
            }
            Some('\\') => {
                out.push('\\');
                i += 2;
            }
            Some(&other) => {
                out.push('\\');
                out.push(other);
                i += 2;
            }
        }
    }
    out
}

/// Scalar dispatch entry, called from `expr.rs` after user functions (so
/// plugins can shadow built-in names) and before the final
/// "no such function" error. Returns `Ok(None)` when `name` is not a
/// regex function.
pub fn call_regex_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    match name {
        // SQLite's REGEXP hook convention: pattern FIRST. (PostgreSQL's
        // family below is source-first — both are documented.)
        "regexp" => {
            if args.len() != 2 {
                return Err(Error::runtime("regexp(pattern, string) takes 2 arguments"));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let pattern = args[0].as_text();
            let subject = args[1].as_text();
            let re = cached_compile(&pattern, false)?;
            let hit = re.find(&subject).is_some();
            Ok(Some(Value::Integer(if hit { 1 } else { 0 })))
        }
        "regexp_like" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(Error::runtime(
                    "regexp_like(source, pattern [, flags]) takes 2-3 arguments",
                ));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let (ci, _) = if let Some(f) = args.get(2) {
                if f.is_null() {
                    return Ok(Some(Value::Null));
                }
                parse_flags(f, false)?
            } else {
                (false, false)
            };
            let source = args[0].as_text();
            let pattern = args[1].as_text();
            let re = cached_compile(&pattern, ci)?;
            let hit = re.find(&source).is_some();
            Ok(Some(Value::Integer(if hit { 1 } else { 0 })))
        }
        "regexp_replace" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(Error::runtime(
                    "regexp_replace(source, pattern, replacement [, flags]) takes 3-4 arguments",
                ));
            }
            if args.iter().take(3).any(|v| v.is_null()) {
                return Ok(Some(Value::Null));
            }
            let (ci, global) = if let Some(f) = args.get(3) {
                if f.is_null() {
                    return Ok(Some(Value::Null));
                }
                parse_flags(f, true)?
            } else {
                (false, false)
            };
            let source = args[0].as_text();
            let pattern = args[1].as_text();
            let repl = args[2].as_text();
            let re = cached_compile(&pattern, ci)?;
            let repl_chars: Vec<char> = repl.chars().collect();
            let chars: Vec<char> = source.chars().collect();
            let mut out = String::new();
            let mut pos = 0usize;
            let mut replaced = false;
            while let Some(m) = find_from(&re, &chars, pos) {
                replaced = true;
                out.push_str(&chars_slice(&chars, pos, m.start));
                out.push_str(&expand_replacement(&repl_chars, &m, &chars));
                pos = m.end;
                if !global {
                    break;
                }
                if m.start == m.end {
                    // Empty match: advance one char (PG behavior) to avoid
                    // an infinite loop, copying the char as-is.
                    if let Some(&c) = chars.get(pos) {
                        out.push(c);
                    }
                    pos += 1;
                    if pos > chars.len() {
                        break;
                    }
                }
                if pos >= chars.len() && m.start == m.end {
                    break;
                }
            }
            if !replaced {
                return Ok(Some(Value::Text(source.into())));
            }
            if pos <= chars.len() {
                out.push_str(&chars_slice(&chars, pos, chars.len()));
            }
            Ok(Some(Value::Text(out.into())))
        }
        "regexp_substr" => {
            if args.len() < 2 || args.len() > 4 {
                return Err(Error::runtime(
                    "regexp_substr(source, pattern [, start [, occurrence]]) takes 2-4 arguments",
                ));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let start = match args.get(2) {
                None | Some(Value::Null) => 0,
                Some(v) => char_start_arg(v, "regexp_substr")?,
            };
            let occ = match args.get(3) {
                None | Some(Value::Null) => 1,
                Some(v) => occurrence_arg(v, "regexp_substr")?,
            };
            let source = args[0].as_text();
            let pattern = args[1].as_text();
            let re = cached_compile(&pattern, false)?;
            let chars: Vec<char> = source.chars().collect();
            if start > chars.len() {
                return Ok(Some(Value::Null));
            }
            let mut pos = start;
            for nth in 1..=occ {
                let Some(raw) = find_from(&re, &chars, pos) else {
                    return Ok(Some(Value::Null));
                };
                if nth == occ {
                    return Ok(Some(Value::Text(
                        chars_slice(&chars, raw.start, raw.end).into(),
                    )));
                }
                pos = raw.end.max(raw.start + 1);
                if pos > chars.len() {
                    return Ok(Some(Value::Null));
                }
            }
            Ok(Some(Value::Null))
        }
        "regexp_instr" => {
            if args.len() < 2 || args.len() > 4 {
                return Err(Error::runtime(
                    "regexp_instr(source, pattern [, start [, occurrence]]) takes 2-4 arguments",
                ));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let start = match args.get(2) {
                None | Some(Value::Null) => 0,
                Some(v) => char_start_arg(v, "regexp_instr")?,
            };
            let occ = match args.get(3) {
                None | Some(Value::Null) => 1,
                Some(v) => occurrence_arg(v, "regexp_instr")?,
            };
            let source = args[0].as_text();
            let pattern = args[1].as_text();
            let re = cached_compile(&pattern, false)?;
            let chars: Vec<char> = source.chars().collect();
            if start > chars.len() {
                return Ok(Some(Value::Integer(0)));
            }
            let mut pos = start;
            for nth in 1..=occ {
                let Some(raw) = find_from(&re, &chars, pos) else {
                    return Ok(Some(Value::Integer(0)));
                };
                if nth == occ {
                    return Ok(Some(Value::Integer((raw.start + 1) as i64)));
                }
                pos = raw.end.max(raw.start + 1);
                if pos > chars.len() {
                    return Ok(Some(Value::Integer(0)));
                }
            }
            Ok(Some(Value::Integer(0)))
        }
        "regexp_count" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(Error::runtime(
                    "regexp_count(source, pattern [, start]) takes 2-3 arguments",
                ));
            }
            if args[0].is_null() || args[1].is_null() {
                return Ok(Some(Value::Null));
            }
            let start = match args.get(2) {
                None | Some(Value::Null) => 0,
                Some(v) => char_start_arg(v, "regexp_count")?,
            };
            let source = args[0].as_text();
            let pattern = args[1].as_text();
            let re = cached_compile(&pattern, false)?;
            let chars: Vec<char> = source.chars().collect();
            if start > chars.len() {
                return Ok(Some(Value::Integer(0)));
            }
            let mut pos = start;
            let mut count: i64 = 0;
            loop {
                if pos > chars.len() {
                    break;
                }
                let Some(raw) = find_from(&re, &chars, pos) else {
                    break;
                };
                count += 1;
                pos = raw.end.max(raw.start + 1);
            }
            Ok(Some(Value::Integer(count)))
        }
        _ => Ok(None),
    }
}

// ============================================================================
// Unit tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str) -> Option<(usize, usize)> {
        Regex::compile(pattern, false)
            .unwrap()
            .find(text)
            .map(|x| (x.start, x.end))
    }

    fn mci(pattern: &str, text: &str) -> Option<(usize, usize)> {
        Regex::compile(pattern, true)
            .unwrap()
            .find(text)
            .map(|x| (x.start, x.end))
    }

    fn g(pattern: &str, text: &str, gid: usize) -> Option<(usize, usize)> {
        Regex::compile(pattern, false)
            .unwrap()
            .find(text)
            .and_then(|x| x.groups.get(gid).copied().flatten())
    }

    #[test]
    fn literals() {
        assert_eq!(m("abc", "xxabcyy"), Some((2, 5)));
        assert_eq!(m("abc", "abd"), None);
        assert_eq!(m("", "anything"), Some((0, 0)));
        assert_eq!(m("a", ""), None);
    }

    #[test]
    fn dot_star_plus() {
        assert_eq!(m("a.c", "abc"), Some((0, 3)));
        assert_eq!(m("a.c", "a\nc"), Some((0, 3))); // . matches newline (POSIX sans REG_NEWLINE)
        assert_eq!(m("ab*c", "ac"), Some((0, 2)));
        assert_eq!(m("ab*c", "abbbc"), Some((0, 5)));
        assert_eq!(m("ab+c", "ac"), None);
        assert_eq!(m("a?b", "b"), Some((0, 1)));
    }

    #[test]
    fn leftmost_longest() {
        // POSIX: longest wins — the Perl engine would return (0,1) for
        // the first alternative `a`.
        assert_eq!(m("a|ab", "ab"), Some((0, 2)));
        assert_eq!(m("(a|ab)(c|bc)", "abc"), Some((0, 3)));
        // leftmost START beats a longer match starting later: "yy" at 0
        // wins over "x" at 2.
        assert_eq!(m("x|yy", "yyx"), Some((0, 2)));
    }

    #[test]
    fn alternation_groups() {
        assert_eq!(m("cat|dog", "hotdog"), Some((3, 6)));
        assert_eq!(g("(a+)(b+)", "xaabbb", 1), Some((1, 3)));
        assert_eq!(g("(a+)(b+)", "xaabbb", 2), Some((3, 6)));
        // non-participating group
        assert_eq!(g("(a)|(b)", "b", 1), None);
        assert_eq!(g("(a)|(b)", "b", 2), Some((0, 1)));
    }

    #[test]
    fn classes() {
        assert_eq!(m("[abc]+", "xxcab"), Some((2, 5)));
        assert_eq!(m("[^0-9]+", "ab12"), Some((0, 2)));
        assert_eq!(m("[a-z]{3}", "zzABabcd"), Some((4, 7)));
        assert_eq!(m("[]a]+", "a]a"), Some((0, 3)));
        // a leading `-` is a literal member: the class contains '-'.
        assert_eq!(m("[-az]+", "-az-"), Some((0, 4)));
        assert_eq!(m("[[:digit:]]+", "ab123"), Some((2, 5)));
        assert_eq!(m("[[:alpha:]]+", "12abc!"), Some((2, 5)));
        // backslash is a LITERAL inside brackets (strict POSIX): `a\-z`
        // is 'a' plus the RANGE '\'..'z' — so '-' (0x2D) is NOT in the
        // class, but '\\' and 'z' are.
        assert_eq!(m(r"[a\-z]+", "a-z"), Some((0, 1)));
        assert_eq!(m(r"[a\-z]+", r"a\z"), Some((0, 3)));
        // complement of a bracket
        assert_eq!(m("[^]]+", "ab]cd"), Some((0, 2)));
    }

    #[test]
    fn anchors() {
        assert_eq!(m("^abc", "abc"), Some((0, 3)));
        assert_eq!(m("^abc", "xabc"), None);
        assert_eq!(m("abc$", "xxabc"), Some((2, 5)));
        assert_eq!(m("abc$", "abcx"), None);
        assert_eq!(m("^$", ""), Some((0, 0)));
        // mid-pattern anchors never match (POSIX whole-string anchors)
        assert_eq!(m("a^b", "a^b"), None);
    }

    #[test]
    fn escapes() {
        assert_eq!(m(r"a\.b", "a.b"), Some((0, 3)));
        assert_eq!(m(r"a\.b", "axb"), None);
        assert_eq!(m(r"\(a\)", "(a)"), Some((0, 3)));
        assert_eq!(m(r"a\tb", "a\tb"), Some((0, 3)));
        assert_eq!(m(r"\d+", "ab42"), Some((2, 4)));
        assert_eq!(m(r"\w+", "!!ab_"), Some((2, 5)));
        assert_eq!(m(r"\s\w", "x y"), Some((1, 3)));
    }

    #[test]
    fn intervals() {
        assert_eq!(m("^a{3}$", "aaa"), Some((0, 3)));
        assert_eq!(m("^a{3}$", "aa"), None);
        assert_eq!(m("^a{2,}$", "aaaaa"), Some((0, 5)));
        assert_eq!(m("^a{2,4}$", "aaa"), Some((0, 3)));
        assert_eq!(m("^a{2,4}$", "aaaaa"), None);
        assert_eq!(m("^(ab){2,3}$", "ababab"), Some((0, 6)));
        // literal brace when not an interval: the whole 4-char string
        // matches (a, '{', 'x', '}' are all literals).
        assert_eq!(m("a{x}", "a{x}"), Some((0, 4)));
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(mci("ABC", "xxabcxx"), Some((2, 5)));
        assert_eq!(mci("[a-z]+", "ABC"), Some((0, 3)));
        assert_eq!(m("ABC", "abc"), None);
    }

    #[test]
    fn complex() {
        // word-ish
        assert_eq!(m(r"^\w+@\w+\.\w+$", "user@site.com"), Some((0, 13)));
        assert_eq!(m(r"^\w+@\w+\.\w+$", "user@site"), None);
        // hex color
        assert_eq!(m("^#[0-9a-fA-F]{6}$", "#1a2B3c"), Some((0, 7)));
        assert_eq!(m("^#[0-9a-fA-F]{6}$", "#1a2B3"), None);
        // nested quantifiers are fine when syntactically grouped
        assert_eq!(m("(ab)*c", "ababc"), Some((0, 5)));
        assert_eq!(m("(a|b)*c", "abbac"), Some((0, 5)));
    }

    #[test]
    fn parse_errors() {
        assert!(Regex::compile("a**", false).is_err());
        assert!(Regex::compile("*a", false).is_err());
        assert!(Regex::compile("(a", false).is_err());
        assert!(Regex::compile("a)", false).is_err());
        assert!(Regex::compile("[a", false).is_err());
        assert!(Regex::compile("a{3,1}", false).is_err());
        assert!(Regex::compile(r"a\", false).is_err());
        assert!(Regex::compile("[z-a]", false).is_err());
    }

    #[test]
    fn linear_no_catastrophe() {
        // The classic catastrophic backtracking pattern — must complete
        // (linear-time NFA, no backtracking).
        let re = Regex::compile("(a+)+b", false).unwrap();
        let subject = "a".repeat(60);
        assert!(re.find(&subject).is_none());
    }

    #[test]
    fn sqlite_regexp_function() {
        let r = call_regex_function(
            "regexp",
            &[Value::Text("^[0-9]+$".into()), Value::Text("12345".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(1));
        let r = call_regex_function(
            "regexp",
            &[Value::Text("^[0-9]+$".into()), Value::Text("12a45".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(0));
        // NULL propagation
        let r = call_regex_function("regexp", &[Value::Null, Value::Text("x".into())])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Null);
    }

    #[test]
    fn pg_functions() {
        // regexp_like + flags
        let r = call_regex_function(
            "regexp_like",
            &[
                Value::Text("Hello World".into()),
                Value::Text("world".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(0));
        let r = call_regex_function(
            "regexp_like",
            &[
                Value::Text("Hello World".into()),
                Value::Text("world".into()),
                Value::Text("i".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(1));

        // regexp_replace: PG default = first occurrence only
        let r = call_regex_function(
            "regexp_replace",
            &[
                Value::Text("a-b-c".into()),
                Value::Text("b".into()),
                Value::Text("X".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("a-X-c".into()));
        // global
        let r = call_regex_function(
            "regexp_replace",
            &[
                Value::Text("a-b-c".into()),
                Value::Text("[bc]".into()),
                Value::Text("X".into()),
                Value::Text("g".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("a-X-X".into()));
        // backreferences: group 1 is "Wall," (greedy — the comma rides
        // along), exactly as PostgreSQL resolves it.
        let r = call_regex_function(
            "regexp_replace",
            &[
                Value::Text("Wall, Jerry".into()),
                Value::Text("(.*) (.*)".into()),
                Value::Text(r"\2 \1".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("Jerry Wall,".into()));
        // \& whole match
        let r = call_regex_function(
            "regexp_replace",
            &[
                Value::Text("ab".into()),
                Value::Text("b".into()),
                Value::Text(r"<\&>".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("a<b>".into()));

        // regexp_substr
        let r = call_regex_function(
            "regexp_substr",
            &[
                Value::Text("2024-05-01".into()),
                Value::Text("[0-9]+".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("2024".into()));
        let r = call_regex_function(
            "regexp_substr",
            &[
                Value::Text("2024-05-01".into()),
                Value::Text("[0-9]+".into()),
                Value::Integer(6),
                Value::Integer(2),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Text("01".into()));

        // regexp_instr
        let r = call_regex_function(
            "regexp_instr",
            &[Value::Text("xxabcxx".into()), Value::Text("abc".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(3));
        let r = call_regex_function(
            "regexp_instr",
            &[Value::Text("xxabcxx".into()), Value::Text("zzz".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(0));

        // regexp_count
        let r = call_regex_function(
            "regexp_count",
            &[
                Value::Text("a1b22c333".into()),
                Value::Text("[0-9]+".into()),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(3));

        // unknown name -> None
        let r = call_regex_function("nope", &[]).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn unicode_positions() {
        // char positions, not bytes. 'b+' matches the LEFTMOST 'b' — the
        // one at index 2, not the run after the euro sign.
        let re = Regex::compile("b+", false).unwrap();
        let m = re.find("aab€bb").unwrap();
        assert_eq!(m.start, 2);
        assert_eq!(m.end, 3);
        let r = call_regex_function(
            "regexp_instr",
            &[Value::Text("€b".into()), Value::Text("b".into())],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Integer(2));
    }
}
