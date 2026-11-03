//! Text-level constraint edits on a stored `CREATE TABLE` statement, for
//! `ALTER TABLE ... DROP CONSTRAINT`, `ALTER [COLUMN] x SET / DROP NOT
//! NULL` and `ADD [CONSTRAINT n] CHECK (...)`.
//!
//! SQLite edits the schema text in place (alter.c: dropConstraintFunc,
//! addConstraintFunc, findConstraintFunc, alterRtrimConstraint) — the
//! user's spelling, spacing and comments survive and `sqlite_schema.sql`
//! comes out byte-identical. This is a port of those token walks over a
//! minimal SQLite-compatible tokenizer.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tk {
    Space,
    Comment,
    Lp,
    Rp,
    Comma,
    /// End of input or an unterminated quote / comment.
    Illegal,
    Constraint,
    Primary,
    Not,
    Unique,
    Check,
    Default,
    Collate,
    References,
    Foreign,
    As,
    Generated,
    Other,
}

fn is_id(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// One token at the start of `z`: (byte length, kind). Length 0 only at
/// end of input.
fn get_token(z: &[u8]) -> (usize, Tk) {
    let Some(&c) = z.first() else {
        return (0, Tk::Illegal);
    };
    match c {
        b' ' | b'\t' | b'\n' | b'\x0c' | b'\r' => {
            let n = z
                .iter()
                .position(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r'))
                .unwrap_or(z.len());
            (n, Tk::Space)
        }
        b'-' if z.get(1) == Some(&b'-') => (
            z.iter().position(|&c| c == b'\n').unwrap_or(z.len()),
            Tk::Comment,
        ),
        b'/' if z.get(1) == Some(&b'*') => match z[2..].windows(2).position(|w| w == b"*/") {
            Some(p) => (p + 4, Tk::Comment),
            None => (z.len(), Tk::Comment),
        },
        b'(' => (1, Tk::Lp),
        b')' => (1, Tk::Rp),
        b',' => (1, Tk::Comma),
        b'\'' | b'"' | b'`' | b'[' => {
            let close = if c == b'[' { b']' } else { c };
            let mut i = 1;
            while i < z.len() {
                if z[i] == close {
                    if close != b']' && z.get(i + 1) == Some(&close) {
                        i += 2;
                        continue;
                    }
                    return (i + 1, Tk::Other);
                }
                i += 1;
            }
            (z.len(), Tk::Illegal)
        }
        c if is_id(c) => {
            let n = z.iter().position(|&c| !is_id(c)).unwrap_or(z.len());
            let kind = match std::str::from_utf8(&z[..n])
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str()
            {
                "CONSTRAINT" => Tk::Constraint,
                "PRIMARY" => Tk::Primary,
                "NOT" => Tk::Not,
                "UNIQUE" => Tk::Unique,
                "CHECK" => Tk::Check,
                "DEFAULT" => Tk::Default,
                "COLLATE" => Tk::Collate,
                "REFERENCES" => Tk::References,
                "FOREIGN" => Tk::Foreign,
                "AS" => Tk::As,
                "GENERATED" => Tk::Generated,
                _ => Tk::Other,
            };
            (n, kind)
        }
        _ => (1, Tk::Other),
    }
}

/// Bytes of leading whitespace / comments.
fn get_whitespace(z: &[u8]) -> usize {
    let mut n = 0;
    loop {
        let (len, t) = get_token(&z[n..]);
        if !matches!(t, Tk::Space | Tk::Comment) {
            return n;
        }
        n += len;
    }
}

/// The next non-space token; a parenthesized group counts as one `Lp`
/// token (getConstraintToken).
fn get_constraint_token(z: &[u8]) -> (usize, Tk) {
    let mut off = 0;
    let mut t;
    loop {
        let (n, k) = get_token(&z[off..]);
        off += n;
        t = k;
        if !matches!(t, Tk::Space | Tk::Comment) {
            break;
        }
    }
    if t == Tk::Lp {
        let mut nest = 1;
        while nest > 0 {
            let (n, k) = get_token(&z[off..]);
            off += n;
            match k {
                Tk::Lp => nest += 1,
                Tk::Rp => {
                    t = Tk::Lp;
                    nest -= 1;
                }
                Tk::Illegal => {
                    t = Tk::Illegal;
                    break;
                }
                _ => {}
            }
        }
    }
    (off, t)
}

/// Bytes from just past a constraint's first token to its end
/// (getConstraint): up to the next token that can start a constraint,
/// a `)` or a `,`.
fn get_constraint(z: &[u8], first: Tk) -> usize {
    let mut off = 0;
    let mut first = first;
    if first == Tk::Foreign {
        off = get_constraint(z, Tk::Other);
        let (n, t) = get_constraint_token(&z[off..]);
        off += n;
        first = t;
    }
    if first == Tk::References {
        off += get_constraint_token(&z[off..]).0;
    }
    loop {
        let (n, t) = get_constraint_token(&z[off..]);
        if matches!(
            t,
            Tk::Constraint
                | Tk::Primary
                | Tk::Not
                | Tk::Unique
                | Tk::Check
                | Tk::Default
                | Tk::Collate
                | Tk::References
                | Tk::Foreign
                | Tk::Rp
                | Tk::Comma
                | Tk::Illegal
                | Tk::As
                | Tk::Generated
        ) {
            return off;
        }
        off += n;
    }
}

/// Offset just past the `(` that opens the column list.
fn skip_create_table(z: &[u8]) -> Option<usize> {
    let mut off = 0;
    loop {
        let (n, t) = get_token(&z[off..]);
        off += n;
        match t {
            Tk::Lp => return Some(off),
            Tk::Illegal => return None,
            _ => {}
        }
    }
}

/// A (possibly quoted) name token, dequoted.
fn dequote(tok: &[u8]) -> String {
    let s = String::from_utf8_lossy(tok);
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (open, close) = (b[0], b[b.len() - 1]);
        let q = match open {
            b'\'' | b'"' | b'`' => Some(open),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(q) = q {
            if close == q {
                let inner = &s[1..s.len() - 1];
                return if open == b'[' {
                    inner.to_string()
                } else {
                    let qs = (q as char).to_string();
                    inner.replace(&qs.repeat(2), &qs)
                };
            }
        }
    }
    s.into_owned()
}

/// What `drop_constraint` removes.
pub enum DropTarget<'a> {
    /// The constraint named this (`DROP CONSTRAINT name`).
    Name(&'a str),
    /// The NOT NULL of the column at this index (`DROP NOT NULL`).
    NotNull(usize),
}

/// The CREATE TABLE text with one constraint removed (dropConstraintFunc).
/// A missing NOT NULL is not an error (the text comes back unchanged); a
/// missing name is `no such constraint: x`, and a named UNIQUE / PRIMARY
/// KEY / DEFAULT ... is `constraint may not be dropped: x`.
pub fn drop_constraint(sql: &str, target: DropTarget<'_>) -> Result<String, String> {
    let z = sql.as_bytes();
    let (cons, not_null) = match target {
        DropTarget::Name(n) => (Some(n), None),
        DropTarget::NotNull(i) => (None, Some(i)),
    };
    let Some(mut off) = skip_create_table(z) else {
        return Err("database disk image is malformed".into());
    };
    let mut start = 0usize;
    let mut end: isize = 0;
    let mut ii = 0usize;
    while end == 0 {
        loop {
            start = off;
            let (n, t) = get_constraint_token(&z[off..]);
            off += n;
            if t == Tk::Constraint && (cons.is_some() || not_null == Some(ii)) {
                off += get_whitespace(&z[off..]);
                let (n_tok, _) = get_constraint_token(&z[off..]);
                let matches = cons.is_some_and(|c| {
                    // An unterminated quote never matches (quotedCompare).
                    let (_, kind) = get_token(&z[off..]);
                    kind != Tk::Illegal && dequote(&z[off..off + n_tok]).eq_ignore_ascii_case(c)
                });
                off += n_tok;
                let (n_tok, mut t) = get_constraint_token(&z[off..]);
                if matches!(
                    t,
                    Tk::Constraint
                        | Tk::Default
                        | Tk::Collate
                        | Tk::Comma
                        | Tk::Rp
                        | Tk::Generated
                        | Tk::As
                ) {
                    // A bare `CONSTRAINT name` (its body belongs to no
                    // droppable constraint): only the name goes.
                    t = Tk::Check;
                } else {
                    off += n_tok;
                    off += get_constraint(&z[off..], t);
                }
                if matches || (not_null.is_some() && t == Tk::Not) {
                    if !matches!(t, Tk::Not | Tk::Check | Tk::References | Tk::Foreign) {
                        return Err(format!(
                            "constraint may not be dropped: {}",
                            cons.unwrap_or("")
                        ));
                    }
                    end = off as isize;
                    break;
                }
            } else if t == Tk::Not && not_null == Some(ii) {
                end = (off + get_constraint(&z[off..], Tk::Other)) as isize;
                break;
            } else if matches!(t, Tk::Rp | Tk::Illegal) {
                end = -1;
                break;
            } else if t == Tk::Comma {
                break;
            }
        }
        ii += 1;
    }
    if end <= 0 {
        return match cons {
            Some(c) => Err(format!("no such constraint: {c}")),
            None => Ok(sql.to_string()),
        };
    }
    let mut end = end as usize;
    end += get_whitespace(&z[end..]);
    let mut space = " ";
    if matches!(get_token(&z[end..]).1, Tk::Rp | Tk::Comma) {
        space = "";
        if start > 0 && z[start - 1] == b',' {
            start -= 1;
        }
    }
    Ok(format!("{}{}{}", &sql[..start], space, &sql[end..]))
}

/// The CREATE TABLE text with `cons` appended to column `col`'s
/// definition, or (`None`) as a new table constraint at the end of the
/// list (addConstraintFunc). `None` when the text is not a CREATE TABLE.
pub fn add_constraint(sql: &str, cons: &str, col: Option<usize>) -> Option<String> {
    let z = sql.as_bytes();
    let mut off = skip_create_table(z)?;
    let mut t = Tk::Other;
    let mut ii = 0usize;
    while col.map_or(t != Tk::Rp, |c| ii <= c) {
        off += get_constraint_token(&z[off..]).0;
        loop {
            let (n, k) = get_constraint_token(&z[off..]);
            t = k;
            if matches!(t, Tk::Comma | Tk::Rp) {
                break;
            }
            if t == Tk::Illegal {
                return None;
            }
            off += n;
        }
        ii += 1;
    }
    off += get_whitespace(&z[off..]);
    let comma = if col.is_none() { "," } else { "" };
    Some(format!("{}{} {}{}", &sql[..off], comma, cons, &sql[off..]))
}

/// Does the CREATE TABLE text name a constraint `name`
/// (findConstraintFunc)?
pub fn has_constraint(sql: &str, name: &str) -> bool {
    let z = sql.as_bytes();
    let mut off = 0;
    loop {
        let (n, t) = get_token(&z[off..]);
        off += n;
        match t {
            Tk::Lp => break,
            Tk::Illegal => return false,
            _ => {}
        }
    }
    loop {
        let (n, t) = get_constraint_token(&z[off..]);
        off += n;
        match t {
            Tk::Constraint => {
                off += get_whitespace(&z[off..]);
                let (n_tok, _) = get_constraint_token(&z[off..]);
                let illegal = get_token(&z[off..]).1 == Tk::Illegal;
                if !illegal && dequote(&z[off..off + n_tok]).eq_ignore_ascii_case(name) {
                    return true;
                }
            }
            Tk::Illegal => return false,
            _ => {}
        }
    }
}

/// `text` without trailing whitespace and `--` comments (`/* */`
/// comments stay) — alterRtrimConstraint.
pub fn rtrim_constraint(text: &str) -> &str {
    let z = text.as_bytes();
    let (mut off, mut end) = (0, 0);
    loop {
        let (n, t) = get_token(&z[off..]);
        if t == Tk::Illegal {
            break;
        }
        if t != Tk::Space && !(t == Tk::Comment && z[off] == b'-') {
            end = off + n;
        }
        off += n;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_named_and_not_null() {
        let d = |sql: &str, n: &str| drop_constraint(sql, DropTarget::Name(n));
        assert_eq!(
            d(
                "CREATE TABLE t1(a, b CONSTRAINT abc CHECK(t1.a != t1.b))",
                "abc"
            )
            .unwrap(),
            "CREATE TABLE t1(a, b)"
        );
        assert_eq!(
            d(
                "CREATE TABLE t1(a, b CONSTRAINT abc CHECK(t1.a != t1.b)NOT NULL)",
                "abc"
            )
            .unwrap(),
            "CREATE TABLE t1(a, b NOT NULL)"
        );
        assert_eq!(
            d(
                "CREATE TABLE t1(a, b,CONSTRAINT abc CHECK(t1.a != t1.b),PRIMARY KEY(a))",
                "abc"
            )
            .unwrap(),
            "CREATE TABLE t1(a, b,PRIMARY KEY(a))"
        );
        assert_eq!(
            d("CREATE TABLE t1(a, b, c, CONSTRAINT abc CONSTRAINT one CHECK(a!=b) CONSTRAINT three)", "abc").unwrap(),
            "CREATE TABLE t1(a, b, c, CONSTRAINT one CHECK(a!=b) CONSTRAINT three)"
        );
        assert_eq!(
            d("CREATE TABLE t1(a, b, c CONSTRAINT abc AS (b+1))", "abc").unwrap(),
            "CREATE TABLE t1(a, b, c AS (b+1))"
        );
        assert_eq!(
            d("CREATE TABLE t2(x, y CONSTRAINT ccc UNIQUE)", "ccc").unwrap_err(),
            "constraint may not be dropped: ccc"
        );
        assert_eq!(
            d("CREATE TABLE t2(x, y CONSTRAINT ccc UNIQUE)", "ddd").unwrap_err(),
            "no such constraint: ddd"
        );
        let nn = |sql: &str, i: usize| drop_constraint(sql, DropTarget::NotNull(i)).unwrap();
        assert_eq!(
            nn("CREATE TABLE t1(a NOT NULL ON CONFLICT FAIL UNIQUE, b)", 0),
            "CREATE TABLE t1(a UNIQUE, b)"
        );
        assert_eq!(
            nn("CREATE TABLE t1(a CHECK(a<b) CONSTRAINT nn NOT NULL, b)", 0),
            "CREATE TABLE t1(a CHECK(a<b), b)"
        );
        assert_eq!(
            nn(
                "CREATE TABLE t1(a, b NOT NULL GENERATED ALWAYS AS (a+1))",
                1
            ),
            "CREATE TABLE t1(a, b GENERATED ALWAYS AS (a+1))"
        );
        assert_eq!(nn("CREATE TABLE t2(x, y)", 0), "CREATE TABLE t2(x, y)");
    }

    #[test]
    fn add_and_find() {
        assert_eq!(
            add_constraint(
                "CREATE TABLE t1(a, 'a b c' VARCHAR(10), UNIQUE(a))",
                "NOT NULL",
                Some(1)
            )
            .unwrap(),
            "CREATE TABLE t1(a, 'a b c' VARCHAR(10) NOT NULL, UNIQUE(a))"
        );
        assert_eq!(
            add_constraint("CREATE TABLE t1(a, b  )", "CHECK (a>=0)", None).unwrap(),
            "CREATE TABLE t1(a, b  , CHECK (a>=0))"
        );
        assert!(has_constraint(
            "CREATE TABLE b1(a, b, CONSTRAINT abc CHECK (a!=2))",
            "ABC"
        ));
        assert!(!has_constraint(
            "CREATE TABLE b1(a, b, CHECK (a!=2))",
            "abc"
        ));
        assert_eq!(
            rtrim_constraint("NOT   NULL ON CONFLICT IGNORE  -- x\n "),
            "NOT   NULL ON CONFLICT IGNORE"
        );
        assert_eq!(rtrim_constraint("CHECK (a) /* c */ "), "CHECK (a) /* c */");
    }
}
