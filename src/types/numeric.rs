//! SQLite's text → number conversion machinery, ported verbatim in
//! semantics from `util.c` / `vdbe.c` / `vdbemem.c`:
//!
//! * [`atof`]  — `sqlite3AtoF`: longest-valid-prefix REAL parse plus the
//!   classification return code every caller keys its decision on;
//! * [`atoi64`] — `sqlite3Atoi64`: prefix INTEGER parse with the exact
//!   overflow / trailing-text return codes;
//! * [`numeric_type`] — vdbe.c `computeNumericType`: the INTEGER-vs-REAL
//!   decision arithmetic operators make for a TEXT/BLOB operand;
//! * [`apply_numeric_affinity`] — vdbe.c `applyNumericAffinity` (with
//!   `bTryForInt`), the column-affinity conversion of TEXT.
//!
//! Everything works on raw BYTES: a BLOB operand is interpreted through
//! the same routines (SQLite reads the blob's bytes as text in the
//! database encoding), and nothing here accepts Rust-only spellings such
//! as `inf`, `NaN` or `infinity` (Rust's `f64::from_str` does — the root
//! of the "text 'Inf' became an infinite REAL" class of divergences).

/// `sqlite3Isspace`: exactly the six ASCII whitespace bytes.
#[inline]
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

const LARGEST_UINT64: u64 = u64::MAX;

/// `sqlite3AtoF` (UTF-8). Returns `(value, rc)`:
///
/// * `rc == 1`  — the whole input (modulo surrounding whitespace) is a pure
///   integer literal;
/// * `rc >= 2`  — the whole input is a well-formed number with a decimal
///   point and/or an exponent (2 or 3);
/// * `rc == 0`  — not a valid number (the value is still the longest
///   valid prefix, 0.0 when there is none);
/// * `rc == -1` — not a valid number, but the valid prefix carries a
///   decimal point and/or an exponent.
///
/// Like SQLite, the significand keeps at most ~19 significant digits
/// (later digits only shift the exponent); the remaining computation is
/// correctly rounded.
pub(crate) fn atof(z: &[u8]) -> (f64, i32) {
    // 3.53's AtoF reads its input as a C string: an embedded NUL ends it
    // (`'3' || x'00410042'` takes INTEGER affinity as 3, and `+ 0` reads
    // it as the REAL 3.0 — the Atoi64 side below does NOT stop there,
    // so the integer classification sees "excess text").
    let z = match z.iter().position(|&b| b == 0) {
        Some(p) => &z[..p],
        None => z,
    };
    let n = z.len();
    if n == 0 {
        return (0.0, 0);
    }
    let mut i = 0usize;
    while i < n && is_space(z[i]) {
        i += 1;
    }
    if i >= n {
        return (0.0, 0);
    }
    let mut sign = 1i32;
    if z[i] == b'-' {
        sign = -1;
        i += 1;
    } else if z[i] == b'+' {
        i += 1;
    }
    let mut s: u64 = 0;
    let mut d: i32 = 0;
    let mut esign: i32 = 1;
    let mut e: i32 = 0;
    let mut e_valid = true;
    let mut n_digit = 0i32;
    let mut e_type = 1i32;

    'parse: {
        while i < n && z[i].is_ascii_digit() {
            s = s * 10 + u64::from(z[i] - b'0');
            i += 1;
            n_digit += 1;
            if s >= (LARGEST_UINT64 - 9) / 10 {
                // Non-significant integer digits only shift the exponent.
                while i < n && z[i].is_ascii_digit() {
                    i += 1;
                    d += 1;
                }
            }
        }
        if i >= n {
            break 'parse;
        }
        if z[i] == b'.' {
            i += 1;
            e_type += 1;
            while i < n && z[i].is_ascii_digit() {
                if s < (LARGEST_UINT64 - 9) / 10 {
                    s = s * 10 + u64::from(z[i] - b'0');
                    d -= 1;
                    n_digit += 1;
                }
                i += 1;
            }
        }
        if i >= n {
            break 'parse;
        }
        if z[i] == b'e' || z[i] == b'E' {
            i += 1;
            e_valid = false;
            e_type += 1;
            if i >= n {
                break 'parse;
            }
            if z[i] == b'-' {
                esign = -1;
                i += 1;
            } else if z[i] == b'+' {
                i += 1;
            }
            while i < n && z[i].is_ascii_digit() {
                e = if e < 10000 {
                    e * 10 + i32::from(z[i] - b'0')
                } else {
                    10000
                };
                i += 1;
                e_valid = true;
            }
        }
        while i < n && is_space(z[i]) {
            i += 1;
        }
    }

    let value = if s == 0 {
        if sign < 0 {
            -0.0
        } else {
            0.0
        }
    } else {
        let exp = e * esign + d;
        // s * 10^exp, correctly rounded (Rust's decimal parser is exact).
        let r: f64 = format!("{s}e{exp}").parse().unwrap_or(0.0);
        let r = if r.is_nan() { f64::INFINITY } else { r };
        if sign < 0 {
            -r
        } else {
            r
        }
    };

    let rc = if i == n && n_digit > 0 && e_valid && e_type > 0 {
        e_type
    } else if e_type >= 2 && (e_type == 3 || e_valid) && n_digit > 0 {
        -1
    } else {
        0
    };
    (value, rc)
}

/// `sqlite3Atoi64` (decimal only). Returns `(value, rc)`:
///
/// * `-1` no digits at all, `0` success (fits, nothing but whitespace
///   after), `1` excess non-space text after the integer, `2` too large
///   (clamped), `3` exactly 9223372036854775808 (clamped to i64::MAX).
pub(crate) fn atoi64(z: &[u8]) -> (i64, i32) {
    let n = z.len();
    let mut i = 0usize;
    while i < n && is_space(z[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < n {
        if z[i] == b'-' {
            neg = true;
            i += 1;
        } else if z[i] == b'+' {
            i += 1;
        }
    }
    let z_start = i;
    while i < n && z[i] == b'0' {
        i += 1;
    }
    let digits_at = i;
    let mut u: u64 = 0;
    let mut j = digits_at;
    while j < n && z[j].is_ascii_digit() {
        u = u.wrapping_mul(10).wrapping_add(u64::from(z[j] - b'0'));
        j += 1;
    }
    let nd = j - digits_at;
    let mut value: i64 = if u > i64::MAX as u64 {
        if neg {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if neg {
        -(u as i64)
    } else {
        u as i64
    };
    let mut rc = 0;
    if nd == 0 && z_start == digits_at {
        rc = -1;
    } else if j < n && z[j..].iter().any(|&b| !is_space(b)) {
        rc = 1;
    }
    if nd < 19 {
        return (value, rc);
    }
    // 19+ significant digits: compare against 9223372036854775808.
    let c = if nd > 19 {
        1
    } else {
        let pow63 = b"9223372036854775808";
        let mut c = 0i32;
        for k in 0..18 {
            c = i32::from(z[digits_at + k]) - i32::from(pow63[k]);
            if c != 0 {
                c *= 10;
                break;
            }
        }
        if c == 0 {
            c = i32::from(z[digits_at + 18]) - i32::from(b'8');
        }
        c
    };
    if c < 0 {
        return (value, rc);
    }
    value = if neg { i64::MIN } else { i64::MAX };
    if c > 0 {
        (value, 2)
    } else if neg {
        (value, rc)
    } else {
        (value, 3)
    }
}

/// `sqlite3RealToI64`: clamping REAL → INTEGER.
#[inline]
pub(crate) fn real_to_i64(r: f64) -> i64 {
    if r < -9223372036854774784.0 {
        i64::MIN
    } else if r > 9223372036854774784.0 {
        i64::MAX
    } else {
        r as i64
    }
}

/// `sqlite3RealSameAsInt`.
#[inline]
pub(crate) fn real_same_as_int(r: f64, i: i64) -> bool {
    r == 0.0
        || (r.to_bits() == (i as f64).to_bits()
            && (-2251799813685248..2251799813685248).contains(&i))
}

/// The numeric reading of a TEXT/BLOB operand for arithmetic (vdbe.c
/// `computeNumericType`): `Ok(i)` when it reads as an INTEGER, `Err(r)`
/// when it reads as a REAL.
pub(crate) fn numeric_type(z: &[u8]) -> Result<i64, f64> {
    let (r, rc) = atof(z);
    if rc <= 0 {
        if rc == 0 {
            let (ix, irc) = atoi64(z);
            if irc <= 1 {
                return Ok(ix);
            }
        }
        Err(r)
    } else if rc == 1 {
        let (ix, irc) = atoi64(z);
        if irc == 0 {
            Ok(ix)
        } else {
            Err(r)
        }
    } else {
        Err(r)
    }
}

/// Outcome of applying NUMERIC/INTEGER/REAL column affinity to TEXT
/// (vdbe.c `applyNumericAffinity(pRec, bTryForInt=1)`): `None` when the
/// text does not look like a number (it keeps its TEXT storage class).
pub(crate) enum NumericAffinity {
    Integer(i64),
    Real(f64),
}

pub(crate) fn apply_numeric_affinity(z: &[u8]) -> Option<NumericAffinity> {
    let (r, rc) = atof(z);
    if rc <= 0 {
        return None;
    }
    if rc == 1 {
        // alsoAnInt
        let ix = real_to_i64(r);
        if real_same_as_int(r, ix) {
            return Some(NumericAffinity::Integer(ix));
        }
        let (i, irc) = atoi64(z);
        if irc == 0 {
            return Some(NumericAffinity::Integer(i));
        }
    }
    // MEM_Real + sqlite3VdbeIntegerAffinity (bTryForInt).
    let ix = real_to_i64(r);
    if r == ix as f64 && ix > i64::MIN && ix < i64::MAX {
        Some(NumericAffinity::Integer(ix))
    } else {
        Some(NumericAffinity::Real(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atof_return_codes() {
        assert_eq!(atof(b"12"), (12.0, 1));
        assert_eq!(atof(b" 12 "), (12.0, 1));
        assert_eq!(atof(b"1.5"), (1.5, 2));
        assert_eq!(atof(b"1e3"), (1000.0, 2));
        assert_eq!(atof(b"1.5e3"), (1500.0, 3));
        assert_eq!(atof(b"12abc"), (12.0, 0));
        assert_eq!(atof(b"1.5abc"), (1.5, -1));
        assert_eq!(atof(b"abc").1, 0);
        assert_eq!(atof(b"Inf"), (0.0, 0));
        assert_eq!(atof(b"NaN"), (0.0, 0));
        assert_eq!(atof(b"1e").1, 0);
        assert_eq!(atof(b".5"), (0.5, 2));
        assert_eq!(atof(b"5."), (5.0, 2));
        assert_eq!(atof(b"1e400").0, f64::INFINITY);
        assert_eq!(atof(b"-0").0.to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn atoi64_return_codes() {
        assert_eq!(atoi64(b"12"), (12, 0));
        assert_eq!(atoi64(b" -12 "), (-12, 0));
        assert_eq!(atoi64(b"12abc"), (12, 1));
        assert_eq!(atoi64(b"abc"), (0, -1));
        assert_eq!(atoi64(b"9223372036854775807"), (i64::MAX, 0));
        assert_eq!(atoi64(b"9223372036854775808"), (i64::MAX, 3));
        assert_eq!(atoi64(b"-9223372036854775808"), (i64::MIN, 0));
        assert_eq!(atoi64(b"99999999999999999999"), (i64::MAX, 2));
        assert_eq!(atoi64(b"00012"), (12, 0));
        assert_eq!(atoi64(b"1.9"), (1, 1));
    }

    #[test]
    fn numeric_type_decisions() {
        assert_eq!(numeric_type(b"12abc"), Ok(12));
        assert_eq!(numeric_type(b"abc"), Ok(0));
        assert_eq!(numeric_type(b"1.5"), Err(1.5));
        assert_eq!(
            numeric_type(b"9223372036854775808"),
            Err(9223372036854775808.0)
        );
    }
}
