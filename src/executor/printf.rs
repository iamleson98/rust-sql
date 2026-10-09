//! SQL `printf()` / `format()` — a port of SQLite's `sqlite3_str_vappendf`
//! in `SQLITE_PRINTF_SQLFUNC` mode (printf.c), driven by SQL values:
//!
//! * integer conversions `%d %i %u %x %X %o %p` (+ the undocumented
//!   ordinal `%r`) with the `- + space # 0 , l ll` flags, width and
//!   precision (`*` takes the next argument);
//! * floating conversions `%f %e %E %g %G` through SQLite's own digit
//!   generator ([`crate::types::fptext::fp_decode`]: 16 significant
//!   digits, 26 with the `!` flag), `Inf`/`NaN` spellings included;
//! * `%s %z`, `%c` (with the repeat-precision quirk), `%q %Q %w`
//!   (quote-doubling / SQL-literal quoting), `%n` (no-op), `%%`;
//! * an unknown conversion character ENDS the output (SQLite returns at
//!   `etINVALID`), exactly like the C implementation.
//!
//! Arguments are consumed like `getIntArg` / `getDoubleArg` /
//! `getTextArg`: missing arguments read as 0 / 0.0 / NULL, and values
//! convert with SQLite's `sqlite3_value_int64` / `_double` / `_text`.
//!
//! Output is accumulated as BYTES (precision and width count bytes unless
//! the `!` flag asks for characters, as in C) and converted to text with
//! lossy UTF-8 repair only if a byte-precision cut split a character.

use crate::types::Value;

struct Args<'a> {
    vals: &'a [Value],
    used: usize,
}

impl Args<'_> {
    fn int(&mut self) -> i64 {
        match self.vals.get(self.used) {
            Some(v) => {
                self.used += 1;
                v.as_integer()
            }
            None => 0,
        }
    }
    fn real(&mut self) -> f64 {
        match self.vals.get(self.used) {
            Some(v) => {
                self.used += 1;
                v.as_real()
            }
            None => 0.0,
        }
    }
    /// `None` = SQL NULL or a missing argument (a NULL `char*`).
    fn text(&mut self) -> Option<Vec<u8>> {
        match self.vals.get(self.used) {
            Some(v) => {
                self.used += 1;
                match v {
                    Value::Null => None,
                    Value::Blob(b) => Some(b.clone()),
                    Value::Text(t) => Some(t.as_bytes().to_vec()),
                    other => Some(other.as_text().into_bytes()),
                }
            }
            None => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Decimal,
    Radix,
    Pointer,
    Ordinal,
    Float,
    Exp,
    Generic,
    Str,
    Char,
    EscQ,
    EscQQ,
    EscW,
    Size,
    Percent,
}

/// (conversion, base, signed, kind, charset offset, prefix offset) —
/// printf.c `fmtinfo[]`.
fn info(c: u8) -> Option<(u32, bool, Kind, usize, usize)> {
    Some(match c {
        b'd' | b'i' => (10, true, Kind::Decimal, 0, 0),
        b's' | b'z' => (0, false, Kind::Str, 0, 0),
        b'g' => (0, true, Kind::Generic, 30, 0),
        b'q' => (0, false, Kind::EscQ, 0, 0),
        b'Q' => (0, false, Kind::EscQQ, 0, 0),
        b'w' => (0, false, Kind::EscW, 0, 0),
        b'c' => (0, false, Kind::Char, 0, 0),
        b'o' => (8, false, Kind::Radix, 0, 2),
        b'u' => (10, false, Kind::Decimal, 0, 0),
        b'x' => (16, false, Kind::Radix, 16, 1),
        b'X' => (16, false, Kind::Radix, 0, 4),
        b'f' => (0, true, Kind::Float, 0, 0),
        b'e' => (0, true, Kind::Exp, 30, 0),
        b'E' => (0, true, Kind::Exp, 14, 0),
        b'G' => (0, true, Kind::Generic, 14, 0),
        b'n' => (0, false, Kind::Size, 0, 0),
        b'%' => (0, false, Kind::Percent, 0, 0),
        b'p' => (16, false, Kind::Pointer, 0, 1),
        b'r' => (10, true, Kind::Ordinal, 0, 0),
        _ => return None,
    })
}

const DIGITS: &[u8; 32] = b"0123456789ABCDEF0123456789abcdef";
const PREFIX: &[u8; 6] = b"-x0\0X0";

/// Run SQLite's printf over `fmt` with SQL argument values.
pub(crate) fn sql_printf(fmt: &[u8], vals: &[Value]) -> Value {
    let mut out: Vec<u8> = Vec::with_capacity(fmt.len() + 16);
    let mut args = Args { vals, used: 0 };
    let n = fmt.len();
    let at = |i: usize| -> u8 { fmt.get(i).copied().unwrap_or(0) };
    let mut i = 0usize;
    'outer: while i < n {
        let c0 = fmt[i];
        if c0 != b'%' {
            let start = i;
            while i < n && fmt[i] != b'%' {
                i += 1;
            }
            out.extend_from_slice(&fmt[start..i]);
            if i >= n {
                break;
            }
        }
        // fmt[i] == '%'
        i += 1;
        let mut c = at(i);
        if c == 0 {
            out.push(b'%');
            break;
        }
        let mut left = false;
        let mut prefix_flag: u8 = 0;
        let mut alt = false;
        let mut alt2 = false;
        let mut zeropad = false;
        let mut thousand = false;
        let mut width: i64 = 0;
        let mut precision: i64 = -1;
        let mut done = false;
        loop {
            match c {
                b'-' => left = true,
                b'+' => prefix_flag = b'+',
                b' ' => prefix_flag = b' ',
                b'#' => alt = true,
                b'!' => alt2 = true,
                b'0' => zeropad = true,
                b',' => thousand = true,
                b'l' => {
                    i += 1;
                    c = at(i);
                    if c == b'l' {
                        i += 1;
                        c = at(i);
                    }
                    done = true;
                }
                b'1'..=b'9' => {
                    let mut wx: u64 = u64::from(c - b'0');
                    loop {
                        i += 1;
                        c = at(i);
                        if c.is_ascii_digit() {
                            wx = (wx * 10 + u64::from(c - b'0')) & 0xffff_ffff_ffff;
                        } else {
                            break;
                        }
                    }
                    width = (wx & 0x7fff_ffff) as i64;
                    if c != b'.' && c != b'l' {
                        done = true;
                    } else {
                        i -= 1;
                    }
                }
                b'*' => {
                    let mut w = args.int();
                    if w < 0 {
                        left = true;
                        w = if w >= -2147483647 { -w } else { 0 };
                    }
                    width = w.min(i64::from(i32::MAX));
                    let nx = at(i + 1);
                    if nx != b'.' && nx != b'l' {
                        i += 1;
                        c = at(i);
                        done = true;
                    }
                }
                b'.' => {
                    i += 1;
                    c = at(i);
                    if c == b'*' {
                        let mut p = args.int();
                        if p < 0 {
                            p = if p >= -2147483647 { -p } else { -1 };
                        }
                        precision = p.min(i64::from(i32::MAX));
                        i += 1;
                        c = at(i);
                    } else {
                        let mut px: u64 = 0;
                        while c.is_ascii_digit() {
                            px = (px * 10 + u64::from(c - b'0')) & 0xffff_ffff_ffff;
                            i += 1;
                            c = at(i);
                        }
                        precision = (px & 0x7fff_ffff) as i64;
                    }
                    if c == b'l' {
                        i -= 1;
                    } else {
                        done = true;
                    }
                }
                _ => done = true,
            }
            if done {
                break;
            }
            i += 1;
            c = at(i);
            if c == 0 {
                break;
            }
        }
        // `c` is the conversion character; `i` points at it.
        let Some((base, signed, kind, charset, prefix_off)) = info(c) else {
            // etINVALID: SQLite stops formatting entirely.
            break 'outer;
        };
        i += 1;
        // Sanity caps (SQLite has SQLITE_PRINTF_PRECISION_LIMIT unset by
        // default; a huge width would be a memory bomb in either engine).
        let width = width.min(100_000_000) as usize;
        let precision = precision.min(100_000_000);
        let mut buf: Vec<u8> = Vec::new();
        // For string-ish conversions the UTF-8 width adjustment applies.
        let mut adjust_utf8 = false;
        let mut width = width;
        match kind {
            Kind::Decimal | Kind::Radix | Kind::Pointer | Kind::Ordinal => {
                let thousand = thousand && kind == Kind::Decimal;
                let (mut longvalue, prefix): (u64, u8) = if signed {
                    let v = args.int();
                    if v < 0 {
                        ((!(v as u64)).wrapping_add(1), b'-')
                    } else {
                        (v as u64, prefix_flag)
                    }
                } else {
                    (args.int() as u64, 0)
                };
                let alt = alt && longvalue != 0;
                let mut precision = precision;
                if zeropad && precision < width as i64 - i64::from(prefix != 0) {
                    precision = width as i64 - i64::from(prefix != 0);
                }
                let mut digits: Vec<u8> = Vec::new(); // reversed
                if kind == Kind::Ordinal {
                    const ORD: &[u8; 8] = b"thstndrd";
                    let mut x = (longvalue % 10) as usize;
                    if x >= 4 || (longvalue / 10) % 10 == 1 {
                        x = 0;
                    }
                    digits.push(ORD[x * 2 + 1]);
                    digits.push(ORD[x * 2]);
                }
                let b = u64::from(base);
                loop {
                    digits.push(DIGITS[charset + (longvalue % b) as usize]);
                    longvalue /= b;
                    if longvalue == 0 {
                        break;
                    }
                }
                let mut length = digits.len() as i64;
                while precision > length {
                    digits.push(b'0');
                    length += 1;
                }
                let mut body: Vec<u8> = digits.into_iter().rev().collect();
                if thousand {
                    let len = body.len();
                    let mut with = Vec::with_capacity(len + len / 3);
                    for (k, d) in body.iter().enumerate() {
                        with.push(*d);
                        let rem = len - 1 - k;
                        if rem > 0 && rem % 3 == 0 {
                            with.push(b',');
                        }
                    }
                    body = with;
                }
                if prefix != 0 {
                    buf.push(prefix);
                }
                if alt && prefix_off != 0 {
                    // aPrefix is stored reversed ("x0" → "0x").
                    let mut p = prefix_off;
                    let mut pre = Vec::new();
                    while PREFIX[p] != 0 {
                        pre.push(PREFIX[p]);
                        p += 1;
                    }
                    pre.reverse();
                    // The C prepends the prefix BEFORE the sign was
                    // emitted only for unsigned conversions (prefix is 0
                    // there), so ordering is: [0x][sign] never mixes.
                    let mut tmp = pre;
                    tmp.extend_from_slice(&buf);
                    buf = tmp;
                }
                buf.extend_from_slice(&body);
            }
            Kind::Float | Kind::Exp | Kind::Generic => {
                let realvalue = args.real();
                let mut precision = if precision < 0 { 6 } else { precision };
                let i_round = match kind {
                    Kind::Float => -precision,
                    Kind::Generic => {
                        if precision == 0 {
                            precision = 1;
                        }
                        precision
                    }
                    _ => precision + 1,
                };
                let mx_round = if alt2 { 26 } else { 16 };
                let i_round = i_round.clamp(-(i32::MAX as i64), i32::MAX as i64) as i32;
                if realvalue.is_nan() {
                    buf.extend_from_slice(if zeropad { b"null" } else { b"NaN" });
                } else if realvalue.is_infinite() && !zeropad {
                    if realvalue < 0.0 {
                        buf.extend_from_slice(b"-Inf");
                    } else {
                        if prefix_flag != 0 {
                            buf.push(prefix_flag);
                        }
                        buf.extend_from_slice(b"Inf");
                    }
                } else {
                    let (sign_neg, digits, i_dp) = if realvalue.is_infinite() {
                        // zeropad Inf renders as 9.0e+999.
                        (realvalue < 0.0, vec![b'9'], 1000i32)
                    } else {
                        let d = crate::types::fptext::fp_decode(realvalue, i_round, mx_round);
                        (d.sign == b'-', d.digits().to_vec(), d.i_dp)
                    };
                    let prefix = if sign_neg { b'-' } else { prefix_flag };
                    let mut xtype = kind;
                    let mut exp = i_dp - 1;
                    if xtype == Kind::Generic && precision > 0 {
                        precision -= 1;
                    }
                    let flag_rtz;
                    if xtype == Kind::Generic {
                        flag_rtz = !alt;
                        if exp < -4 || i64::from(exp) > precision {
                            xtype = Kind::Exp;
                        } else {
                            precision -= i64::from(exp);
                            xtype = Kind::Float;
                        }
                    } else {
                        flag_rtz = alt2;
                    }
                    let mut e2: i64 = if xtype == Kind::Exp {
                        0
                    } else {
                        i64::from(i_dp) - 1
                    };
                    let flag_dp = precision > 0 || alt || alt2;
                    if prefix != 0 {
                        buf.push(prefix);
                    }
                    let start = buf.len();
                    let mut j = 0usize;
                    let nd = digits.len();
                    if e2 < 0 {
                        buf.push(b'0');
                    } else {
                        while e2 >= 0 {
                            buf.push(if j < nd {
                                j += 1;
                                digits[j - 1]
                            } else {
                                b'0'
                            });
                            if thousand && e2 % 3 == 0 && e2 > 1 {
                                buf.push(b',');
                            }
                            e2 -= 1;
                        }
                    }
                    if flag_dp {
                        buf.push(b'.');
                    }
                    e2 += 1;
                    while e2 < 0 && precision > 0 {
                        buf.push(b'0');
                        precision -= 1;
                        e2 += 1;
                    }
                    while precision > 0 {
                        precision -= 1;
                        buf.push(if j < nd {
                            j += 1;
                            digits[j - 1]
                        } else {
                            b'0'
                        });
                    }
                    if flag_rtz && flag_dp {
                        while buf.len() > start && *buf.last().unwrap() == b'0' {
                            buf.pop();
                        }
                        if buf.last() == Some(&b'.') {
                            if alt2 {
                                buf.push(b'0');
                            } else {
                                buf.pop();
                            }
                        }
                    }
                    if xtype == Kind::Exp {
                        exp = i_dp - 1;
                        if nd == 1 && digits[0] == b'0' {
                            exp = 0;
                        }
                        buf.push(DIGITS[charset]);
                        if exp < 0 {
                            buf.push(b'-');
                            exp = -exp;
                        } else {
                            buf.push(b'+');
                        }
                        if exp >= 100 {
                            buf.push(b'0' + (exp / 100) as u8);
                            exp %= 100;
                        }
                        buf.push(b'0' + (exp / 10) as u8);
                        buf.push(b'0' + (exp % 10) as u8);
                    }
                    if zeropad && !left && buf.len() < width {
                        let n_pad = width - buf.len();
                        let at = usize::from(prefix != 0);
                        let pad = vec![b'0'; n_pad];
                        buf.splice(at..at, pad);
                    }
                }
            }
            Kind::Size => {
                width = 0;
            }
            Kind::Percent => buf.push(b'%'),
            Kind::Char => {
                let mut one: Vec<u8> = Vec::new();
                match args.text() {
                    Some(t) if !t.is_empty() => {
                        one.push(t[0]);
                        if t[0] & 0xc0 == 0xc0 {
                            let mut k = 1;
                            while one.len() < 4 && k < t.len() && t[k] & 0xc0 == 0x80 {
                                one.push(t[k]);
                                k += 1;
                            }
                        }
                    }
                    _ => one.push(0),
                }
                if precision > 1 {
                    let reps = precision as usize;
                    let w = width as i64 - (precision - 1);
                    if w > 1 && !left {
                        out.extend(std::iter::repeat(b' ').take((w - 1) as usize));
                        width = 0;
                    } else {
                        width = w.max(0) as usize;
                    }
                    for _ in 0..reps - 1 {
                        out.extend_from_slice(&one);
                    }
                }
                buf = one;
                alt2 = true;
                adjust_utf8 = true;
            }
            Kind::Str => {
                let s = args.text().unwrap_or_default();
                let s: &[u8] = match s.iter().position(|&b| b == 0) {
                    Some(z) => &s[..z],
                    None => &s,
                };
                let len = if precision >= 0 {
                    if alt2 {
                        let mut k = 0usize;
                        let mut p = precision;
                        while p > 0 && k < s.len() {
                            p -= 1;
                            k += 1;
                            while k < s.len() && s[k] & 0xc0 == 0x80 {
                                k += 1;
                            }
                        }
                        k
                    } else {
                        (precision as usize).min(s.len())
                    }
                } else {
                    s.len()
                };
                buf.extend_from_slice(&s[..len]);
                adjust_utf8 = true;
            }
            Kind::EscQ | Kind::EscQQ | Kind::EscW => {
                let q = if kind == Kind::EscW { b'"' } else { b'\'' };
                let raw = args.text();
                let isnull = raw.is_none();
                let escarg: Vec<u8> = match raw {
                    Some(t) => t,
                    None => {
                        if kind == Kind::EscQQ {
                            b"NULL".to_vec()
                        } else {
                            b"(NULL)".to_vec()
                        }
                    }
                };
                let escarg: &[u8] = match escarg.iter().position(|&b| b == 0) {
                    Some(z) => &escarg[..z],
                    None => &escarg,
                };
                // Precision limits the INPUT bytes (characters with `!`).
                let mut k = precision;
                let mut take = 0usize;
                while k != 0 && take < escarg.len() {
                    if alt2 && escarg[take] & 0xc0 == 0xc0 {
                        while take + 1 < escarg.len() && escarg[take + 1] & 0xc0 == 0x80 {
                            take += 1;
                        }
                    }
                    take += 1;
                    k -= 1;
                }
                let need_quote = !isnull && kind == Kind::EscQQ;
                if need_quote {
                    buf.push(q);
                }
                for &ch in &escarg[..take] {
                    buf.push(ch);
                    if ch == q {
                        buf.push(ch);
                    }
                }
                if need_quote {
                    buf.push(q);
                }
                adjust_utf8 = true;
            }
        }
        if adjust_utf8 && alt2 && width > 0 {
            width += buf.iter().filter(|&&b| b & 0xc0 == 0x80).count();
        }
        if width > buf.len() {
            let pad = width - buf.len();
            if !left {
                out.extend(std::iter::repeat(b' ').take(pad));
            }
            out.extend_from_slice(&buf);
            if left {
                out.extend(std::iter::repeat(b' ').take(pad));
            }
        } else {
            out.extend_from_slice(&buf);
        }
    }
    match String::from_utf8(out) {
        Ok(s) => Value::Text(s.into()),
        Err(e) => Value::Text(String::from_utf8_lossy(e.as_bytes()).into_owned().into()),
    }
}
