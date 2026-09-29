//! Geopoly — SQLite's geopoly module (polygons over the rtree
//! infrastructure), a faithful port of `geopoly.c` from the bundled
//! amalgamation.
//!
//! The polygon model, the JSON grammar (including its quirks), the
//! float32 coordinate storage, the sweep-line overlap algorithm, the
//! `geopolySine` approximation in `geopoly_regular`, the `%!g`/`%g`
//! float rendering, and every error path are transliterations pinned
//! against the real-SQLite oracle (rusqlite bundled, compiled with
//! `-DSQLITE_ENABLE_GEOPOLY`).
//!
//! Oracle-pinned behaviors worth remembering:
//! - Coordinates are **f32** (`GeoCoord = float`); JSON input parses as
//!   f64 then truncates to f32; `16777217` stores as `16777216`.
//! - TEXT not starting with `[` parses as an INVALID polygon with rc=OK
//!   (`'nope'` inserts fine with a degenerate zero bbox); `[`-prefixed
//!   text that fails the grammar is a hard error
//!   ("_shape does not contain a valid polygon"). Blobs ≥ 28 bytes with
//!   a wrong vertex count are rc=OK too; blobs < 28 bytes are errors.
//! - `geopoly_overlap`/`geopoly_within` in WHERE become bbox PREFILTER
//!   constraints (SQLite's xFindFunction, omit=0) — the exact test is
//!   the re-applied function. The vtab query strategies are 1 (rowid),
//!   2 (overlap bbox), 3 (within bbox), 4 (fullscan).
//! - `geopoly_within(P1,P2)` = 1 iff P1 is inside P2, 2 when identical;
//!   `geopoly_overlap` returns 0/1/2 (P1 within P2) /3 (P2 within P1)
//!   /4 (same polygon).
//! - UPDATE that moves the rowid always fails (SQLite recomputes the
//!   bbox from an argv slot it leaves NULL).

use crate::error::{Error, Result};
use crate::plugin::vtab::{
    IndexInfo, ModuleCaps, ShadowTable, UpdateOp, VirtualTable, VirtualTableCursor,
    VirtualTableModule, VtabConstraint, VtabConstraintOp,
};
use crate::types::Value;
use std::collections::BTreeMap;

// ============================================================================
// SQLite-exact float rendering: %g (svg) and %!g (json)
// ============================================================================
//
// SQLite's printf etGENERIC branch: round to 6 significant digits with
// round-half-UP on the exact decimal expansion (0.09765625 ->
// 0.0976563, where Rust's half-even formatting gives ...62), strip
// trailing zeros, fixed form while -4 <= exp10 <= 5, else exponent form
// with a two-digit exponent. The `!` (alternate-2) flag forces a
// trailing ".0" when the strip would leave a bare "." ("1.0" not "1").

/// SQLite `sqlite3FpDecode` equivalent: the shortest-round-trip decimal
/// digits of `r` (they agree with the exact decimal expansion through
/// 15+ digits — far beyond the 6 we round to), SQLite's rounding applied
/// at `i_round` significant digits (round-half-up with carry), plus the
/// decimal-point position.
struct FpDigits {
    sign: bool,
    digits: Vec<u8>, // ASCII b'0'..b'9'
    i_dp: i32,       // value = 0.digits * 10^i_dp; empty digits = special
    special: bool,   // NaN / Inf
}

fn fp_decode(r: f64, i_round: i32) -> FpDigits {
    if r == 0.0 {
        // Covers -0.0 too: SQLite returns "0" with sign '+'.
        return FpDigits {
            sign: false,
            digits: vec![b'0'],
            i_dp: 1,
            special: false,
        };
    }
    if r.is_nan() || r.is_infinite() {
        return FpDigits {
            sign: r < 0.0,
            digits: Vec::new(),
            i_dp: 0,
            special: true,
        };
    }
    let sign = r < 0.0;
    let a = r.abs();
    // Rust's shortest round-trip rendering: "d.dddde-5".
    let s = format!("{:e}", a);
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let mut digits: Vec<u8> = mant.bytes().filter(|b| b.is_ascii_digit()).collect();
    let mut i_dp = exp + 1;
    // SQLite's rounding at i_round significant digits (only when
    // i_round < n). Round-half-up with carry propagation.
    let n = digits.len() as i32;
    if i_round > 0 && i_round < n {
        let k = i_round as usize;
        if digits[k] >= b'5' {
            let mut j = k - 1;
            loop {
                digits[j] += 1;
                if digits[j] <= b'9' {
                    break;
                }
                digits[j] = b'0';
                if j == 0 {
                    digits.insert(0, b'1');
                    i_dp += 1;
                    break;
                }
                j -= 1;
            }
        }
        digits.truncate(k);
    }
    while digits.len() > 1 && *digits.last().unwrap() == b'0' {
        digits.pop();
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    FpDigits {
        sign,
        digits,
        i_dp,
        special: false,
    }
}

/// SQLite's %g / %!g (etGENERIC): 6 significant digits, half-up,
/// fixed form for -4 <= exp <= 5, else a two-digit exponent; `altform2`
/// (the `!` flag) keeps a trailing ".0".
pub(crate) fn format_g(r: f64, altform2: bool) -> String {
    let mut precision: i32 = 6;
    if precision == 0 {
        precision = 1;
    }
    let s = fp_decode(r, precision);
    if s.special {
        if r.is_nan() {
            return "NaN".to_string();
        }
        return if r < 0.0 { "-Inf" } else { "Inf" }.to_string();
    }
    let mut out = String::new();
    if s.sign {
        out.push('-');
    }
    let exp = s.i_dp - 1;
    let precision = precision - 1; // etGENERIC decrements
    let fixed = exp >= -4 && exp <= precision;
    let (mut prec_used, e2) = if fixed {
        (precision - exp, s.i_dp - 1)
    } else {
        (precision, 0)
    };
    let flag_dp = prec_used > 0 || altform2;
    let mut j = 0usize;
    let mut e2m = e2;
    if e2m < 0 {
        out.push('0');
    } else {
        while e2m >= 0 {
            out.push(if j < s.digits.len() {
                let c = s.digits[j];
                j += 1;
                c as char
            } else {
                '0'
            });
            e2m -= 1;
        }
    }
    if flag_dp {
        out.push('.');
    }
    e2m += 1;
    while e2m < 0 && prec_used > 0 {
        out.push('0');
        prec_used -= 1;
        e2m += 1;
    }
    while prec_used > 0 {
        out.push(if j < s.digits.len() {
            let c = s.digits[j];
            j += 1;
            c as char
        } else {
            '0'
        });
        prec_used -= 1;
    }
    // Trailing-zero strip (flag_rtz), with the altform2 ".0" form.
    if flag_dp {
        while out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            if altform2 {
                out.push('0');
            } else {
                out.pop();
            }
        }
    }
    if !fixed {
        let mut e = s.i_dp - 1;
        out.push('e');
        if e < 0 {
            out.push('-');
            e = -e;
        } else {
            out.push('+');
        }
        if e >= 100 {
            out.push((b'0' + (e / 100) as u8) as char);
            e %= 100;
        }
        out.push((b'0' + (e / 10) as u8) as char);
        out.push((b'0' + (e % 10) as u8) as char);
    }
    out
}

// ============================================================================
// The polygon model
// ============================================================================

/// A polygon: (x, y) pairs stored as **f32** (GeoCoord), X first.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GeoPoly {
    coords: Vec<f32>,
}

#[allow(clippy::excessive_precision, clippy::approx_constant)]
const GEOPOLY_PI: f64 = 3.1415926535897932385;

impl GeoPoly {
    fn n_vertex(&self) -> usize {
        self.coords.len() / 2
    }
    fn x(&self, i: usize) -> f64 {
        self.coords[i * 2] as f64
    }
    fn y(&self, i: usize) -> f64 {
        self.coords[i * 2 + 1] as f64
    }
    fn set_x(&mut self, i: usize, v: f64) {
        self.coords[i * 2] = v as f32;
    }
    fn set_y(&mut self, i: usize, v: f64) {
        self.coords[i * 2 + 1] = v as f32;
    }

    /// The on-disk BLOB: 4-byte header (LE flag, 3-byte BE vertex count)
    /// + f32 pairs, little-endian.
    fn blob(&self) -> Vec<u8> {
        let n = self.n_vertex();
        let mut out = Vec::with_capacity(4 + n * 8);
        out.push(1);
        out.push(((n >> 16) & 0xff) as u8);
        out.push(((n >> 8) & 0xff) as u8);
        out.push((n & 0xff) as u8);
        for c in &self.coords {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out
    }

    fn from_blob(b: &[u8]) -> Option<GeoPoly> {
        if b.len() < 4 + 6 * 4 {
            return None;
        }
        let n_vertex = ((b[1] as usize) << 16) | ((b[2] as usize) << 8) | b[3] as usize;
        if (b[0] != 0 && b[0] != 1) || n_vertex * 2 * 4 + 4 != b.len() {
            return None;
        }
        let mut coords = Vec::with_capacity(n_vertex * 2);
        for i in 0..n_vertex * 2 {
            let mut a = [0u8; 4];
            a.copy_from_slice(&b[4 + i * 4..8 + i * 4]);
            coords.push(if b[0] == 1 {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            });
        }
        Some(GeoPoly { coords })
    }

    /// Shoelace area (negative for clockwise winding). The differences
    /// and their product are f32 (C's `float op float = float` — the
    /// oracle-pinned low bits depend on it), only the `* 0.5` and the
    /// accumulation widen to f64.
    fn area(&self) -> f64 {
        let n = self.n_vertex();
        let mut r_area = 0.0f64;
        for ii in 0..n.saturating_sub(1) {
            let dx = self.coords[ii * 2] - self.coords[(ii + 1) * 2];
            let dy = self.coords[ii * 2 + 1] + self.coords[(ii + 1) * 2 + 1];
            r_area += (dx * dy) as f64 * 0.5;
        }
        if n > 0 {
            let ii = n - 1;
            let dx = self.coords[ii * 2] - self.coords[0];
            let dy = self.coords[ii * 2 + 1] + self.coords[1];
            r_area += (dx * dy) as f64 * 0.5;
        }
        r_area
    }

    /// (mnX, mxX, mnY, mxY) — tracked in f32 like SQLite.
    fn bbox_coords(&self) -> [f32; 4] {
        let n = self.n_vertex();
        let mut mn_x = self.coords[0];
        let mut mx_x = self.coords[0];
        let mut mn_y = self.coords[1];
        let mut mx_y = self.coords[1];
        for ii in 1..n {
            let r = self.x(ii);
            if r < mn_x as f64 {
                mn_x = r as f32;
            } else if r > mx_x as f64 {
                mx_x = r as f32;
            }
            let r = self.y(ii);
            if r < mn_y as f64 {
                mn_y = r as f32;
            } else if r > mx_y as f64 {
                mx_y = r as f32;
            }
        }
        [mn_x, mx_x, mn_y, mx_y]
    }

    fn from_bbox_coords(c: [f32; 4]) -> GeoPoly {
        // CCW from the bottom-left: (mnX,mnY),(mxX,mnY),(mxX,mxY),(mnX,mxY).
        GeoPoly {
            coords: vec![c[0], c[2], c[1], c[2], c[1], c[3], c[0], c[3]],
        }
    }

    fn bbox(&self) -> GeoPoly {
        Self::from_bbox_coords(self.bbox_coords())
    }

    /// Reverse the winding when clockwise (geopoly_ccw).
    fn ccw(&self) -> GeoPoly {
        let mut p = self.clone();
        if p.area() < 0.0 {
            let n = p.n_vertex();
            let mut ii = 1;
            let mut jj = n.saturating_sub(1);
            while ii < jj {
                p.coords.swap(ii * 2, jj * 2);
                p.coords.swap(ii * 2 + 1, jj * 2 + 1);
                ii += 1;
                jj -= 1;
            }
        }
        p
    }
}

// ============================================================================
// JSON parse — geopolyParseJson, quirks included
// ============================================================================

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// geopolyParseJson: a well-formed closed GeoJSON array with >= 4 pairs
/// (>= 3 unique vertices) and nothing but whitespace after. None on any
/// grammar failure.
pub(crate) fn parse_json(z: &str) -> Option<GeoPoly> {
    let b = z.as_bytes();
    let mut i = 0usize;
    let mut coords: Vec<f32> = Vec::new();
    let mut n_vertex = 0usize; // pairs seen (including the closing repeat)

    macro_rules! skip_space {
        ($i:expr) => {
            while $i < b.len() && is_space(b[$i]) {
                $i += 1;
            }
        };
    }

    // parse_number: SQLite's grammar — optional '-', digits, one '.',
    // one exponent; NO leading '+'; "0" never followed by a digit; the
    // last character must be a digit (so "1." fails, ".5" is fine).
    fn parse_number(z: &str, b: &[u8], i: &mut usize) -> Option<f64> {
        let mut p = *i;
        while p < b.len() && is_space(b[p]) {
            p += 1;
        }
        let s = p;
        let mut j = 0usize;
        let mut seen_dp = false;
        let mut seen_e = false;
        if s < b.len() && b[s] == b'-' {
            j = 1;
        }
        // Leading zero rule: '0' (after the sign) followed by a digit.
        let z0 = s + j;
        if z0 < b.len() && b[z0] == b'0' && z0 + 1 < b.len() && b[z0 + 1].is_ascii_digit() {
            return None;
        }
        loop {
            let pos = s + j;
            if pos >= b.len() {
                return None;
            }
            let c = b[pos];
            if c.is_ascii_digit() {
                j += 1;
                continue;
            }
            if c == b'.' {
                // SQLite only rejects "-." (the '-' directly before the
                // '.'); a bare ".5" parses (the byte before the number is
                // whatever preceded it, never '-').
                if j == 1 && b[pos - 1] == b'-' {
                    return None;
                }
                if seen_dp {
                    return None;
                }
                seen_dp = true;
                j += 1;
                continue;
            }
            if c == b'e' || c == b'E' {
                if b[pos - 1] < b'0' {
                    return None;
                }
                if seen_e {
                    return None;
                }
                seen_dp = true;
                seen_e = true;
                let mut k = j + 1;
                if s + k < b.len() && (b[s + k] == b'+' || b[s + k] == b'-') {
                    k += 1;
                }
                if s + k >= b.len() || !b[s + k].is_ascii_digit() {
                    return None;
                }
                j = k;
                continue;
            }
            break;
        }
        if b[s + j - 1] < b'0' {
            return None;
        }
        let text = &z[s..s + j];
        match text.parse::<f64>() {
            Ok(v) => {
                *i = s + j;
                Some(v)
            }
            Err(_) => None,
        }
    }

    skip_space!(i);
    if i >= b.len() || b[i] != b'[' {
        return None;
    }
    i += 1;
    loop {
        skip_space!(i);
        if i < b.len() && b[i] == b'[' {
            i += 1;
            let mut ii = 0usize;
            loop {
                let v = parse_number(z, b, &mut i)?;
                if ii <= 1 {
                    while coords.len() < n_vertex * 2 + 2 {
                        coords.push(0.0);
                    }
                    coords[n_vertex * 2 + ii] = v as f32;
                }
                ii += 1;
                if ii == 2 {
                    n_vertex += 1;
                }
                skip_space!(i);
                let c = if i < b.len() { b[i] } else { 0 };
                i += 1;
                if c == b',' {
                    continue;
                }
                if c == b']' && ii >= 2 {
                    break;
                }
                return None;
            }
            skip_space!(i);
            if i < b.len() && b[i] == b',' {
                i += 1;
                continue;
            }
            break;
        }
        break;
    }
    skip_space!(i);
    let closed = i < b.len()
        && b[i] == b']'
        && n_vertex >= 4
        && coords.len() >= (n_vertex - 1) * 2 + 2
        && coords[0] == coords[(n_vertex - 1) * 2]
        && coords[1] == coords[(n_vertex - 1) * 2 + 1];
    i += 1;
    skip_space!(i);
    if closed && i == b.len() {
        coords.truncate((n_vertex - 1) * 2);
        Some(GeoPoly { coords })
    } else {
        None
    }
}

/// geopolyFuncParam: decode a Value into a polygon.
/// - `Ok(Some(p))` — a valid polygon.
/// - `Ok(None)` — invalid but rc=OK (TEXT not starting with `[`, or a
///   ≥ 28-byte blob with a wrong vertex count): functions return NULL,
///   the bbox is zeros.
/// - `Err(())` — a hard rc=SQLITE_ERROR input (NULL / INTEGER / REAL,
///   `[`-prefixed bad text, a blob < 28 bytes): the INSERT fails with
///   "_shape does not contain a valid polygon" and the scan errors.
pub(crate) fn func_param(v: &Value) -> std::result::Result<Option<GeoPoly>, ()> {
    match v {
        Value::Blob(b) => {
            if b.len() >= 4 + 6 * 4 {
                // The blob path's rc is always OK — even when malformed.
                Ok(GeoPoly::from_blob(b))
            } else {
                Err(())
            }
        }
        Value::Text(t) => {
            let s = t.as_str();
            let mut k = 0;
            let bytes = s.as_bytes();
            while k < bytes.len() && is_space(bytes[k]) {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'[' {
                match parse_json(s) {
                    Some(p) => Ok(Some(p)),
                    None => Err(()), // '['-prefixed malformed text
                }
            } else {
                Ok(None) // not an array: rc stays OK, no polygon
            }
        }
        _ => Err(()), // NULL / INTEGER / REAL
    }
}

// ============================================================================
// Geometry: contains_point, the overlap sweep, regular, sine
// ============================================================================

/// pointBeneathLine: 2 on the segment, 1 beneath, 0 not beneath. The
/// left-most endpoint is excluded (SQLite's parity rule).
fn point_beneath_line(x0: f64, y0: f64, x1: f64, y1: f64, x2: f64, y2: f64) -> i32 {
    if x0 == x1 && y0 == y1 {
        return 2;
    }
    if x1 < x2 {
        if x0 <= x1 || x0 > x2 {
            return 0;
        }
    } else if x1 > x2 {
        if x0 <= x2 || x0 > x1 {
            return 0;
        }
    } else {
        // Vertical segment
        if x0 != x1 {
            return 0;
        }
        if y0 < y1 && y0 < y2 {
            return 0;
        }
        if y0 > y1 && y0 > y2 {
            return 0;
        }
        return 2;
    }
    let y = y1 + (y2 - y1) * (x0 - x1) / (x2 - x1);
    if y0 == y {
        2
    } else if y0 < y {
        1
    } else {
        0
    }
}

fn contains_point(p: &GeoPoly, x0: f64, y0: f64) -> i64 {
    let n = p.n_vertex();
    let mut v = 0i32;
    let mut cnt = 0i32;
    let mut ii = 0usize;
    while ii + 1 < n {
        v = point_beneath_line(x0, y0, p.x(ii), p.y(ii), p.x(ii + 1), p.y(ii + 1));
        if v == 2 {
            break;
        }
        cnt += v;
        ii += 1;
    }
    if v != 2 && n > 0 {
        v = point_beneath_line(x0, y0, p.x(n - 1), p.y(n - 1), p.x(0), p.y(0));
    }
    if v == 2 {
        1
    } else if ((v + cnt) & 1) == 0 {
        0
    } else {
        2
    }
}

/// geopolySine: SQLite's fast approximation, -pi/2 <= r <= 2*pi.
fn geopoly_sine(r: f64) -> f64 {
    let mut r = r;
    if r >= 1.5 * GEOPOLY_PI {
        r -= 2.0 * GEOPOLY_PI;
    }
    if r >= 0.5 * GEOPOLY_PI {
        -geopoly_sine(r - GEOPOLY_PI)
    } else {
        let r2 = r * r;
        let r3 = r2 * r;
        let r5 = r3 * r2;
        0.9996949 * r - 0.1656700 * r3 + 0.0075134 * r5
    }
}

fn geopoly_regular(x: f64, y: f64, r: f64, n: i64) -> Option<GeoPoly> {
    if n < 3 || r <= 0.0 {
        return None;
    }
    let n = n.min(1000) as usize;
    let mut p = GeoPoly {
        coords: vec![0.0; n * 2],
    };
    for i in 0..n {
        let angle = 2.0 * GEOPOLY_PI * (i as f64) / (n as f64);
        p.set_x(i, x - r * geopoly_sine(angle - 0.5 * GEOPOLY_PI));
        p.set_y(i, y + r * geopoly_sine(angle));
    }
    Some(p)
}

// ---- The overlap sweep (geopolyOverlap) — a transliteration --------

#[derive(Clone, Copy)]
struct GeoSegment {
    c: f64,
    b: f64,
    y: f64,
    y0: f32,
    side: u8,
}

#[derive(Clone, Copy)]
struct GeoEvent {
    x: f64,
    e_type: u8, // 0 = ADD, 1 = REMOVE
    seg: u32,
}

fn add_one_segment(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    side: u8,
    events: &mut Vec<GeoEvent>,
    segments: &mut Vec<GeoSegment>,
) {
    if x0 == x1 {
        return; // vertical segments are ignored
    }
    let (x0, y0, x1, y1) = if x0 > x1 {
        (x1, y1, x0, y0)
    } else {
        (x0, y0, x1, y1)
    };
    // The slope is an f32 division (C: float/float = float), stored
    // into the double C — the low bits drive the sweep's y comparisons.
    let c = ((y1 as f32) - (y0 as f32)) / ((x1 as f32) - (x0 as f32));
    let c = c as f64;
    let b = y1 - x1 * c;
    let seg_i = segments.len() as u32;
    segments.push(GeoSegment {
        c,
        b,
        y: 0.0,
        y0: y0 as f32,
        side,
    });
    events.push(GeoEvent {
        x: x0,
        e_type: 0,
        seg: seg_i,
    });
    events.push(GeoEvent {
        x: x1,
        e_type: 1,
        seg: seg_i,
    });
}

fn add_segments(p: &GeoPoly, side: u8, events: &mut Vec<GeoEvent>, segments: &mut Vec<GeoSegment>) {
    let n = p.n_vertex();
    if n == 0 {
        return;
    }
    for i in 0..n - 1 {
        add_one_segment(
            p.x(i),
            p.y(i),
            p.x(i + 1),
            p.y(i + 1),
            side,
            events,
            segments,
        );
    }
    add_one_segment(
        p.x(n - 1),
        p.y(n - 1),
        p.x(0),
        p.y(0),
        side,
        events,
        segments,
    );
}

/// geopolyEventMerge: `pRight->x <= pLeft->x` takes RIGHT first.
fn merge_events(left: &[u32], right: &[u32], events: &[GeoEvent]) -> Vec<u32> {
    let (mut a, mut b) = (0usize, 0usize);
    let mut out = Vec::with_capacity(left.len() + right.len());
    while a < left.len() && b < right.len() {
        if events[right[b] as usize].x <= events[left[a] as usize].x {
            out.push(right[b]);
            b += 1;
        } else {
            out.push(left[a]);
            a += 1;
        }
    }
    out.extend_from_slice(&left[a..]);
    out.extend_from_slice(&right[b..]);
    out
}

/// geopolySortEventsByX: the bottom-up 50-slot merge over the raw array
/// order, folded with the accumulated list as the RIGHT operand.
fn sort_events_by_x(events: &[GeoEvent]) -> Vec<u32> {
    let mut slots: Vec<Option<Vec<u32>>> = Vec::new();
    for e in 0..events.len() as u32 {
        let mut p = vec![e];
        let mut j = 0usize;
        while j < slots.len() {
            let Some(l) = slots[j].take() else {
                break;
            };
            p = merge_events(&l, &p, events);
            j += 1;
        }
        if j == slots.len() {
            slots.push(Some(p));
        } else {
            slots[j] = Some(p);
        }
    }
    let mut acc: Vec<u32> = Vec::new();
    for s in slots.into_iter().flatten() {
        acc = merge_events(&s, &acc, events);
    }
    acc
}

/// geopolySegmentMerge: d = right.y - left.y; on 0, d = right.C - left.C;
/// d < 0 takes RIGHT, else LEFT.
fn merge_segments(left: &[u32], right: &[u32], segs: &[GeoSegment]) -> Vec<u32> {
    let (mut a, mut b) = (0usize, 0usize);
    let mut out = Vec::with_capacity(left.len() + right.len());
    while a < left.len() && b < right.len() {
        let l = &segs[left[a] as usize];
        let r = &segs[right[b] as usize];
        let mut d = r.y - l.y;
        if d == 0.0 {
            d = r.c - l.c;
        }
        if d < 0.0 {
            out.push(right[b]);
            b += 1;
        } else {
            out.push(left[a]);
            a += 1;
        }
    }
    out.extend_from_slice(&left[a..]);
    out.extend_from_slice(&right[b..]);
    out
}

/// geopolySortSegmentsByYAndC over the active list's current order.
fn sort_segments_by_y_and_c(active: &mut Vec<u32>, segs: &[GeoSegment]) {
    let mut slots: Vec<Option<Vec<u32>>> = Vec::new();
    for &e in active.iter() {
        let mut p = vec![e];
        let mut j = 0usize;
        while j < slots.len() {
            let Some(l) = slots[j].take() else {
                break;
            };
            p = merge_segments(&l, &p, segs);
            j += 1;
        }
        if j == slots.len() {
            slots.push(Some(p));
        } else {
            slots[j] = Some(p);
        }
    }
    let mut acc: Vec<u32> = Vec::new();
    for s in slots.into_iter().flatten() {
        acc = merge_segments(&s, &acc, segs);
    }
    *active = acc;
}

/// The overlap code: 0 disjoint, 1 partial, 2 P1 within P2,
/// 3 P2 within P1, 4 identical. Transliterated from geopolyOverlap —
/// including the iMask carry-over between the two active-list passes.
pub(crate) fn geopoly_overlap(p1: &GeoPoly, p2: &GeoPoly) -> i32 {
    let mut events: Vec<GeoEvent> = Vec::new();
    let mut segments: Vec<GeoSegment> = Vec::new();

    add_segments(p1, 1, &mut events, &mut segments);
    add_segments(p2, 2, &mut events, &mut segments);

    // Events sorted by x — geopolySortEventsByX transliterated: a
    // bottom-up 50-slot merge over the raw array order (ADD then REMOVE
    // per segment), geopolyEventMerge takes the RIGHT operand on
    // `right.x <= left.x` ties.
    let sorted = sort_events_by_x(&events);

    let mut active: Vec<u32> = Vec::new();
    let mut need_sort = false;
    let mut a_overlap = [false; 4];
    let mut crossed = false;
    let mut r_x = if let Some(&first) = sorted.first() {
        if events[first as usize].x == 0.0 {
            -1.0
        } else {
            0.0
        }
    } else {
        0.0
    };

    'outer: for &ei in &sorted {
        let ev = events[ei as usize];
        if ev.x != r_x {
            r_x = ev.x;
            if need_sort {
                // geopolySortSegmentsByYAndC transliterated: the same
                // bottom-up 50-slot merge over the active list's order,
                // segmentMerge taking RIGHT on d < 0 and LEFT on ties
                // (d == 0.0 after the (y, C) comparison).
                sort_segments_by_y_and_c(&mut active, &segments);
                need_sort = false;
            }
            // Pass 1: masks from adjacent y gaps. iMask carries into
            // pass 2 (never reset — the C declares it once).
            let mut i_mask = 0u8;
            let mut prev_y: Option<f64> = None;
            for &si in &active {
                let seg = &segments[si as usize];
                if let Some(py) = prev_y {
                    if py != seg.y {
                        a_overlap[i_mask as usize] = true;
                    }
                }
                i_mask ^= seg.side;
                prev_y = Some(seg.y);
            }
            // Pass 2: recompute y at r_x, detect crossings, more masks.
            let mut prev_y: Option<f64> = None;
            let mut prev_side = 0u8;
            for &si in &active {
                let seg = &mut segments[si as usize];
                seg.y = seg.c * r_x + seg.b;
                let y = seg.y;
                let side = seg.side;
                if let Some(py) = prev_y {
                    if py > y && prev_side != side {
                        crossed = true;
                        break 'outer;
                    } else if py != y {
                        a_overlap[i_mask as usize] = true;
                    }
                }
                i_mask ^= side;
                prev_y = Some(y);
                prev_side = side;
            }
        }
        if ev.e_type == 0 {
            // ADD — LIFO like the C's list head insert.
            let seg = &mut segments[ev.seg as usize];
            seg.y = seg.y0 as f64;
            active.insert(0, ev.seg);
            need_sort = true;
        } else if let Some(pos) = active.iter().position(|&s| s == ev.seg) {
            active.remove(pos);
        }
    }

    if crossed {
        return 1;
    }
    if !a_overlap[3] {
        0
    } else if a_overlap[1] && !a_overlap[2] {
        3
    } else if !a_overlap[1] && a_overlap[2] {
        2
    } else if !a_overlap[1] && !a_overlap[2] {
        4
    } else {
        1
    }
}

// ============================================================================
// SQL functions (12 scalars; the aggregate registers in the plugin
// registry)
// ============================================================================

fn arity_error(name: &str) -> Error {
    Error::semantic(format!("wrong number of arguments to function {}()", name))
}

fn one_poly(name: &str, args: &[Value]) -> Result<Option<GeoPoly>> {
    if args.len() != 1 {
        return Err(arity_error(name));
    }
    Ok(func_param(&args[0]).unwrap_or_default()) // hard-error inputs render NULL
}

fn two_polys(name: &str, args: &[Value]) -> Result<Option<(GeoPoly, GeoPoly)>> {
    if args.len() != 2 {
        return Err(arity_error(name));
    }
    let a = func_param(&args[0]).unwrap_or_default();
    let b = func_param(&args[1]).unwrap_or_default();
    match (a, b) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        _ => Ok(None),
    }
}

/// Dispatch the geopoly scalar family. Ok(None) = not a geopoly name.
/// Every result is oracle-pinned: NULL on any invalid input, arity
/// errors with SQLite's exact text.
pub(crate) fn call_geopoly_function(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let fname = name.to_ascii_lowercase();
    let v = match fname.as_str() {
        "geopoly_area" => match one_poly(&fname, args)? {
            Some(p) => Value::Real(p.area()),
            None => Value::Null,
        },
        "geopoly_blob" => match one_poly(&fname, args)? {
            Some(p) => Value::Blob(p.blob()),
            None => Value::Null,
        },
        "geopoly_json" => match one_poly(&fname, args)? {
            Some(p) => Value::Text(json_render(&p).into()),
            None => Value::Null,
        },
        "geopoly_bbox" => match one_poly(&fname, args)? {
            Some(p) => Value::Blob(p.bbox().blob()),
            None => Value::Null,
        },
        "geopoly_ccw" => match one_poly(&fname, args)? {
            Some(p) => Value::Blob(p.ccw().blob()),
            None => Value::Null,
        },
        "geopoly_debug" => {
            if args.len() != 1 {
                return Err(arity_error(&fname));
            }
            Value::Null
        }
        "geopoly_within" => match two_polys(&fname, args)? {
            Some((a, b)) => {
                let x = geopoly_overlap(&a, &b);
                Value::Integer(match x {
                    2 => 1,
                    4 => 2,
                    _ => 0,
                })
            }
            None => Value::Null,
        },
        "geopoly_overlap" => match two_polys(&fname, args)? {
            Some((a, b)) => Value::Integer(geopoly_overlap(&a, &b) as i64),
            None => Value::Null,
        },
        "geopoly_contains_point" => {
            if args.len() != 3 {
                return Err(arity_error(&fname));
            }
            match func_param(&args[0]) {
                Ok(Some(p)) => {
                    Value::Integer(contains_point(&p, args[1].as_real(), args[2].as_real()))
                }
                _ => Value::Null,
            }
        }
        "geopoly_regular" => {
            if args.len() != 4 {
                return Err(arity_error(&fname));
            }
            match geopoly_regular(
                args[0].as_real(),
                args[1].as_real(),
                args[2].as_real(),
                args[3].as_integer(),
            ) {
                Some(p) => Value::Blob(p.blob()),
                None => Value::Null,
            }
        }
        "geopoly_svg" => {
            // Variadic (nArg = -1): argc < 1 returns NULL.
            match args.first() {
                Some(first) => match func_param(first) {
                    Ok(Some(p)) => Value::Text(svg_render(&p, &args[1..]).into()),
                    _ => Value::Null,
                },
                None => Value::Null,
            }
        }
        "geopoly_xform" => {
            if args.len() != 7 {
                return Err(arity_error(&fname));
            }
            match one_poly(&fname, &args[..1])? {
                Some(mut p) => {
                    let a = args[1].as_real();
                    let b = args[2].as_real();
                    let c = args[3].as_real();
                    let d = args[4].as_real();
                    let e = args[5].as_real();
                    let f = args[6].as_real();
                    for i in 0..p.n_vertex() {
                        let x0 = p.x(i);
                        let y0 = p.y(i);
                        p.set_x(i, a * x0 + b * y0 + e);
                        p.set_y(i, c * x0 + d * y0 + f);
                    }
                    Value::Blob(p.blob())
                }
                None => Value::Null,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// geopoly_json: `[[x,y],...,[x0,y0]]` with %!g rendering.
fn json_render(p: &GeoPoly) -> String {
    let mut out = String::from("[");
    for i in 0..p.n_vertex() {
        out.push('[');
        out.push_str(&format_g(p.x(i), true));
        out.push(',');
        out.push_str(&format_g(p.y(i), true));
        out.push_str("],");
    }
    out.push('[');
    out.push_str(&format_g(p.x(0), true));
    out.push(',');
    out.push_str(&format_g(p.y(0), true));
    out.push_str("]]");
    out
}

/// geopoly_svg: `<polyline points='x,y ... x0,y0' attrs></polyline>`
/// with plain %g rendering.
fn svg_render(p: &GeoPoly, attrs: &[Value]) -> String {
    let mut out = String::from("<polyline points=");
    let mut sep = '\'';
    for i in 0..p.n_vertex() {
        out.push(sep);
        out.push_str(&format_g(p.x(i), false));
        out.push(',');
        out.push_str(&format_g(p.y(i), false));
        sep = ' ';
    }
    out.push(' ');
    out.push_str(&format_g(p.x(0), false));
    out.push(',');
    out.push_str(&format_g(p.y(0), false));
    out.push('\'');
    for a in attrs {
        let t = a.as_text();
        if !t.is_empty() {
            out.push(' ');
            out.push_str(t.as_str());
        }
    }
    out.push_str("></polyline>");
    out
}

// ============================================================================
// The geopoly_group_bbox aggregate
// ============================================================================

pub(crate) struct GroupBBox {
    init: bool,
    a: [f32; 4],
}

pub(crate) fn group_bbox_step(state: &mut GroupBBox, arg: &Value) {
    // Invalid inputs are SKIPPED (rc != OK in geopolyBBoxStep).
    let coords: [f32; 4] = match func_param(arg) {
        Ok(Some(p)) => p.bbox_coords(),
        _ => return,
    };
    if !state.init {
        state.init = true;
        state.a = coords;
    } else {
        if coords[0] < state.a[0] {
            state.a[0] = coords[0];
        }
        if coords[1] > state.a[1] {
            state.a[1] = coords[1];
        }
        if coords[2] < state.a[2] {
            state.a[2] = coords[2];
        }
        if coords[3] > state.a[3] {
            state.a[3] = coords[3];
        }
    }
}

pub(crate) fn group_bbox_final(state: &GroupBBox) -> Value {
    if !state.init {
        return Value::Null;
    }
    Value::Blob(GeoPoly::from_bbox_coords(state.a).blob())
}

/// The plugin-registry form (statement-scope aggregate dispatch).
pub(crate) fn group_bbox_function() -> std::sync::Arc<dyn crate::plugin::AggregateFunction> {
    std::sync::Arc::new(GroupBBoxFunction)
}

struct GroupBBoxFunction;

impl crate::plugin::AggregateFunction for GroupBBoxFunction {
    fn name(&self) -> &str {
        "geopoly_group_bbox"
    }
    fn arity(&self) -> crate::plugin::Arity {
        crate::plugin::Arity::Exact(1)
    }
    fn init(&self) -> Box<dyn crate::plugin::AggState> {
        Box::new(GroupBBoxState {
            state: GroupBBox {
                init: false,
                a: [0.0; 4],
            },
        })
    }
}

struct GroupBBoxState {
    state: GroupBBox,
}

impl crate::plugin::AggState for GroupBBoxState {
    fn step(&mut self, _ctx: &crate::plugin::AggCtx, args: &[Value]) -> Result<()> {
        if let Some(a) = args.first() {
            group_bbox_step(&mut self.state, a);
        }
        Ok(())
    }
    fn value(&self) -> Result<Value> {
        Ok(group_bbox_final(&self.state))
    }
}

// ============================================================================
// A compact 2D R-tree over the row bboxes
// ============================================================================

const RT_MAX_ENTRIES: usize = 32;

#[derive(Clone)]
struct Rt2Entry {
    bbox: [f32; 4],              // mnX, mxX, mnY, mxY
    child: Option<Box<Rt2Node>>, // None = leaf (id below)
    id: i64,
}

#[derive(Clone, Default)]
struct Rt2Node {
    entries: Vec<Rt2Entry>,
}

impl Rt2Node {
    fn bbox(&self) -> [f32; 4] {
        let mut b = [0.0f32; 4];
        let mut first = true;
        for e in &self.entries {
            if first {
                b = e.bbox;
                first = false;
            } else {
                b[0] = b[0].min(e.bbox[0]);
                b[1] = b[1].max(e.bbox[1]);
                b[2] = b[2].min(e.bbox[2]);
                b[3] = b[3].max(e.bbox[3]);
            }
        }
        b
    }
}

/// A real (if simple) in-memory R-tree: least-enlargement descent,
/// quadratic split at 32 entries, delete with empty-subtree drop.
#[derive(Clone, Default)]
pub(crate) struct RTree2D {
    root: Rt2Node,
}

fn area(b: &[f32; 4]) -> f64 {
    ((b[1] - b[0]) as f64) * ((b[3] - b[2]) as f64)
}

fn union(a: [f32; 4], b: [f32; 4]) -> [f32; 4] {
    [
        a[0].min(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].max(b[3]),
    ]
}

impl RTree2D {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn insert(&mut self, id: i64, bbox: [f32; 4]) {
        if let Some(split) = Self::insert_rec(&mut self.root, id, bbox, true) {
            let left_bb = self.root.bbox();
            let old = std::mem::take(&mut self.root);
            self.root.entries.push(Rt2Entry {
                bbox: left_bb,
                child: Some(Box::new(old)),
                id: 0,
            });
            self.root.entries.push(split);
        }
    }

    fn insert_rec(node: &mut Rt2Node, id: i64, bbox: [f32; 4], at_leaf: bool) -> Option<Rt2Entry> {
        if at_leaf {
            node.entries.push(Rt2Entry {
                bbox,
                child: None,
                id,
            });
        } else {
            let mut best = 0usize;
            let mut best_cost = f64::INFINITY;
            for (i, e) in node.entries.iter().enumerate() {
                let grown = union(e.bbox, bbox);
                let cost = area(&grown) - area(&e.bbox);
                if cost < best_cost {
                    best_cost = cost;
                    best = i;
                }
            }
            let mut taken = node.entries.swap_remove(best);
            let mut child_bb = taken.bbox;
            let split = match taken.child.as_mut() {
                Some(child) => {
                    let split = Self::insert_rec(child, id, bbox, false);
                    child_bb = child.bbox();
                    split.map(|mut e| {
                        e.bbox = e.child.as_ref().map(|c| c.bbox()).unwrap_or(e.bbox);
                        e
                    })
                }
                None => None,
            };
            taken.bbox = child_bb;
            node.entries.push(taken);
            if let Some(mut e) = split {
                e.bbox = e.child.as_ref().map(|c| c.bbox()).unwrap_or(e.bbox);
                node.entries.push(e);
            }
        }
        if node.entries.len() <= RT_MAX_ENTRIES {
            return None;
        }
        Some(Self::split_node(node))
    }

    fn split_node(node: &mut Rt2Node) -> Rt2Entry {
        let n = node.entries.len();
        let mut s1 = 0usize;
        let mut s2 = 1usize;
        let mut worst = f64::NEG_INFINITY;
        for i in 0..n {
            for j in i + 1..n {
                let u = union(node.entries[i].bbox, node.entries[j].bbox);
                let waste = area(&u) - area(&node.entries[i].bbox) - area(&node.entries[j].bbox);
                if waste > worst {
                    worst = waste;
                    s1 = i;
                    s2 = j;
                }
            }
        }
        let mut entries: Vec<Rt2Entry> = std::mem::take(&mut node.entries);
        let e2 = entries.swap_remove(s2);
        let e1 = entries.swap_remove(s1.min(entries.len()));
        let mut left = Rt2Node { entries: vec![e1] };
        let mut right = Rt2Node { entries: vec![e2] };
        while !entries.is_empty() {
            let e = entries.remove(0);
            let gl = union(left.bbox(), e.bbox);
            let gr = union(right.bbox(), e.bbox);
            let dl = area(&gl) - area(&left.bbox());
            let dr = area(&gr) - area(&right.bbox());
            if dl < dr || (dl == dr && left.entries.len() <= right.entries.len()) {
                left.entries.push(e);
            } else {
                right.entries.push(e);
            }
        }
        node.entries = std::mem::take(&mut left.entries);
        Rt2Entry {
            bbox: right.bbox(),
            child: Some(Box::new(right)),
            id: 0,
        }
    }

    pub(crate) fn remove(&mut self, id: i64) {
        Self::remove_rec(&mut self.root, id);
    }

    fn remove_rec(node: &mut Rt2Node, id: i64) -> bool {
        // true = this subtree no longer contains id.
        let mut i = 0;
        while i < node.entries.len() {
            match &node.entries[i].child {
                None => {
                    if node.entries[i].id == id {
                        node.entries.remove(i);
                        return false;
                    }
                    i += 1;
                }
                Some(_) => {
                    let gone = {
                        let Some(child) = node.entries[i].child.as_mut() else {
                            unreachable!()
                        };
                        Self::remove_rec(child, id)
                    };
                    if gone {
                        node.entries.remove(i);
                        continue;
                    }
                    if let Some(child) = node.entries[i].child.as_mut() {
                        node.entries[i].bbox = child.bbox();
                    }
                    return false;
                }
            }
        }
        node.entries.is_empty()
    }

    /// Cells whose bbox INTERSECTS the query bbox (strategy 2 prefilter).
    pub(crate) fn search_overlap(&self, q: [f32; 4]) -> Vec<i64> {
        let mut out = Vec::new();
        Self::walk_overlap(&self.root, q, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn walk_overlap(node: &Rt2Node, q: [f32; 4], out: &mut Vec<i64>) {
        for e in &node.entries {
            let b = e.bbox;
            if b[0] > q[1] || b[1] < q[0] || b[2] > q[3] || b[3] < q[2] {
                continue; // no intersection
            }
            match &e.child {
                None => out.push(e.id),
                Some(c) => Self::walk_overlap(c, q, out),
            }
        }
    }

    /// Cells FULLY CONTAINED in the query bbox (strategy 3 prefilter).
    pub(crate) fn search_within(&self, q: [f32; 4]) -> Vec<i64> {
        let mut out = Vec::new();
        Self::walk_within(&self.root, q, &mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn walk_within(node: &Rt2Node, q: [f32; 4], out: &mut Vec<i64>) {
        for e in &node.entries {
            let b = e.bbox;
            // A contained cell needs b ⊆ q; prune only when the subtree
            // cannot hold one (no intersection with q at all).
            if b[1] < q[0] || b[0] > q[1] || b[3] < q[2] || b[2] > q[3] {
                continue;
            }
            match &e.child {
                None => {
                    if b[0] >= q[0] && b[1] <= q[1] && b[2] >= q[2] && b[3] <= q[3] {
                        out.push(e.id);
                    }
                }
                Some(c) => Self::walk_within(c, q, out),
            }
        }
    }
}

// ============================================================================
// The virtual table
// ============================================================================

struct GeoConfig {
    table: String,
    /// Aux column declarations, verbatim from USING geopoly(...) — may
    /// carry types (`name TEXT`), which SQLite passes into the declared
    /// schema.
    aux_cols: Vec<String>,
}

#[derive(Clone)]
struct GeoRow {
    /// The stored _shape: a normalized blob for valid TEXT input, the
    /// raw value otherwise (invalid text / malformed blob).
    shape: Value,
    aux: Vec<Value>,
}

struct GeoState {
    cfg: GeoConfig,
    rows: BTreeMap<i64, GeoRow>,
    tree: RTree2D,
}

impl GeoState {
    fn insert_row(&mut self, rowid: i64, shape: Value, aux: Vec<Value>) {
        let bbox = match func_param(&shape) {
            Ok(Some(p)) => p.bbox_coords(),
            _ => [0.0; 4],
        };
        self.rows.insert(rowid, GeoRow { shape, aux });
        self.tree.insert(rowid, bbox);
    }
}

struct GeoTable {
    state: std::sync::Arc<parking_lot::Mutex<GeoState>>,
}

/// Normalize an incoming _shape value like geopolyUpdate's write path:
/// valid TEXT becomes its BLOB form; everything else is stored raw.
fn normalize_shape(shape: &Value) -> Value {
    if let Value::Text(_) = shape {
        if let Ok(Some(p)) = func_param(shape) {
            return Value::Blob(p.blob());
        }
    }
    shape.clone()
}

impl VirtualTable for GeoTable {
    fn columns(&self) -> Vec<(String, String)> {
        let st = self.state.lock();
        let mut cols = vec![("_shape".to_string(), String::new())];
        for a in &st.cfg.aux_cols {
            let t = a.trim();
            match t.find(|c: char| c.is_whitespace()) {
                Some(i) => cols.push((t[..i].trim().to_string(), t[i..].trim().to_string())),
                None => cols.push((t.to_string(), String::new())),
            }
        }
        cols
    }

    fn shadow_tables(&self) -> Vec<ShadowTable> {
        let st = self.state.lock();
        let t = &st.cfg.table;
        let n_aux = st.cfg.aux_cols.len();
        // SQLite's exact shadow layout: t_rowid(rowid IPK, nodeno, a0..),
        // t_node(nodeno, data), t_parent(nodeno, parentnode). The content
        // shadow is t_rowid with a column map (vtab col i → a{i}) — the
        // same table real SQLite creates, so geopoly tables in real
        // SQLite files reindex transparently.
        let mut cols = String::from("nodeno");
        for i in 0..=n_aux {
            cols.push_str(&format!(",a{}", i));
        }
        vec![
            ShadowTable {
                name: format!("{}_rowid", t),
                create_sql: format!(
                    "CREATE TABLE \"{}_rowid\"(rowid INTEGER PRIMARY KEY,{})",
                    t, cols
                ),
                content: true,
                content_map: Some((1..=n_aux + 1).collect()),
            },
            ShadowTable {
                name: format!("{}_node", t),
                create_sql: format!(
                    "CREATE TABLE \"{}_node\"(nodeno INTEGER PRIMARY KEY,data)",
                    t
                ),
                content: false,
                content_map: None,
            },
            ShadowTable {
                name: format!("{}_parent", t),
                create_sql: format!(
                    "CREATE TABLE \"{}_parent\"(nodeno INTEGER PRIMARY KEY,parentnode)",
                    t
                ),
                content: false,
                content_map: None,
            },
        ]
    }

    fn best_index(&self, constraints: &[VtabConstraint]) -> Result<IndexInfo> {
        let mut info = IndexInfo::full_scan(constraints.len());
        let mut rowid_term: Option<usize> = None;
        let mut func_term: Option<usize> = None;
        for (i, c) in constraints.iter().enumerate() {
            match (&c.column, &c.op) {
                (None, VtabConstraintOp::Eq) => {
                    rowid_term = Some(i);
                    break; // SQLite breaks on the first rowid term
                }
                (Some(0), VtabConstraintOp::Function(f)) => {
                    let strategy = match *f {
                        "geopoly_overlap" => 2,
                        "geopoly_within" => 3,
                        _ => 0,
                    };
                    if strategy > 0 {
                        func_term = Some(i);
                    }
                }
                _ => {}
            }
        }
        if let Some(i) = rowid_term {
            info.idx_num = 1;
            info.idx_str = Some("rowid".to_string());
            info.handled[i] = true;
            info.recheck[i] = false;
            info.estimated_cost = 30.0;
            info.estimated_rows = 1;
            return Ok(info);
        }
        if let Some(i) = func_term {
            info.idx_num = match &constraints[i].op {
                VtabConstraintOp::Function(f) if f.eq_ignore_ascii_case("geopoly_within") => 3,
                _ => 2,
            };
            info.idx_str = Some("rtree".to_string());
            info.handled[i] = true;
            // omit=0: the bbox prefilter is NOT exact — the function is
            // re-applied as a residual.
            info.recheck[i] = true;
            info.estimated_cost = 300.0;
            info.estimated_rows = 10;
            return Ok(info);
        }
        info.idx_num = 4;
        info.idx_str = Some("fullscan".to_string());
        info.estimated_cost = 3_000_000.0;
        info.estimated_rows = 100_000;
        Ok(info)
    }

    fn open(&self) -> Result<Box<dyn VirtualTableCursor>> {
        Ok(Box::new(GeoCursor {
            state: self.state.clone(),
            rows: Vec::new(),
            pos: 0,
        }))
    }

    fn update(&mut self, ops: Vec<UpdateOp>) -> Result<Vec<Option<i64>>> {
        let mut st = self.state.lock();
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match (op.old_rowid, op.new_rowid) {
                (None, Some(rid)) => {
                    // INSERT: argv[2] = the _shape value.
                    let shape = op.columns.first().cloned().flatten().unwrap_or(Value::Null);
                    if func_param(&shape).is_err() {
                        return Err(Error::constraint("_shape does not contain a valid polygon"));
                    }
                    let aux = op
                        .columns
                        .iter()
                        .skip(1)
                        .map(|v| v.clone().unwrap_or(Value::Null))
                        .collect();
                    st.insert_row(rid, normalize_shape(&shape), aux);
                    out.push(Some(rid));
                }
                (Some(old), new_rowid) => {
                    if let Some(new) = new_rowid {
                        if new != old {
                            // SQLite recomputes the bbox from a NULL
                            // argv slot on rowid moves — always an error.
                            return Err(Error::constraint(
                                "_shape does not contain a valid polygon",
                            ));
                        }
                    }
                    if op.columns.is_empty() {
                        st.rows.remove(&old);
                        st.tree.remove(old);
                        out.push(None);
                        continue;
                    }
                    // UPDATE: a None column is UNCHANGED — merge with the
                    // old row (SQLite's argv nochange semantics; the
                    // engine passes unmerged assignments).
                    let old_row = st.rows.get(&old).cloned();
                    let shape = match op.columns.first().cloned().flatten() {
                        Some(v) => v,
                        None => old_row
                            .as_ref()
                            .map(|r| r.shape.clone())
                            .unwrap_or(Value::Null),
                    };
                    if func_param(&shape).is_err() {
                        return Err(Error::constraint("_shape does not contain a valid polygon"));
                    }
                    let aux = op
                        .columns
                        .iter()
                        .skip(1)
                        .enumerate()
                        .map(|(i, v)| {
                            v.clone()
                                .or_else(|| old_row.as_ref().and_then(|r| r.aux.get(i).cloned()))
                                .unwrap_or(Value::Null)
                        })
                        .collect();
                    st.rows.remove(&old);
                    st.tree.remove(old);
                    st.insert_row(old, normalize_shape(&shape), aux);
                    out.push(Some(old));
                }
                (None, None) => {
                    return Err(Error::semantic("invalid geopoly update op"));
                }
            }
        }
        Ok(out)
    }

    fn reindex(&mut self, rows: &[(i64, Vec<Value>)]) -> Result<()> {
        let mut st = self.state.lock();
        let mut new_rows = BTreeMap::new();
        let mut tree = RTree2D::new();
        for (rid, vals) in rows {
            let mut it = vals.iter().cloned();
            let shape = it.next().unwrap_or(Value::Null);
            let aux: Vec<Value> = it.collect();
            let bbox = match func_param(&shape) {
                Ok(Some(p)) => p.bbox_coords(),
                _ => [0.0; 4],
            };
            new_rows.insert(*rid, GeoRow { shape, aux });
            tree.insert(*rid, bbox);
        }
        st.rows = new_rows;
        st.tree = tree;
        Ok(())
    }

    fn overloaded_functions(&self) -> &'static [&'static str] {
        &["geopoly_overlap", "geopoly_within"]
    }

    fn shadow_normalize(&self, values: &[Value]) -> Vec<Value> {
        let mut out = values.to_vec();
        if let Some(first) = out.first_mut() {
            *first = normalize_shape(first);
        }
        out
    }

    fn rowid_unique_error(&self) -> Option<String> {
        let st = self.state.lock();
        Some(format!("UNIQUE constraint failed: {}._shape", st.cfg.table))
    }
}

struct GeoCursor {
    state: std::sync::Arc<parking_lot::Mutex<GeoState>>,
    rows: Vec<i64>,
    pos: usize,
}

impl VirtualTableCursor for GeoCursor {
    fn filter(&mut self, idx_num: usize, _idx_str: Option<&str>, args: &[Value]) -> Result<()> {
        self.pos = 0;
        self.rows.clear();
        let st = self.state.lock();
        match idx_num {
            1 => {
                let rid = args.first().map(|v| v.as_integer()).unwrap_or(0);
                if st.rows.contains_key(&rid) {
                    self.rows.push(rid);
                }
            }
            2 | 3 => {
                // Overlap (2) / within (3) bbox search. The query value
                // computes a bbox like geopolyBBox: hard-error inputs
                // (NULL / numbers / short blobs / '['-bad text) fail the
                // scan ("SQL logic error"); rc-OK invalid inputs yield a
                // zero bbox (matching nothing, like SQLite's memset).
                let query = args.first().cloned().unwrap_or(Value::Null);
                let bbox: [f32; 4] = match func_param(&query) {
                    Err(()) => return Err(Error::semantic("SQL logic error")),
                    Ok(Some(p)) => p.bbox_coords(),
                    Ok(None) => [0.0; 4],
                };
                self.rows = if idx_num == 2 {
                    st.tree.search_overlap(bbox)
                } else {
                    st.tree.search_within(bbox)
                };
            }
            _ => {
                self.rows = st.rows.keys().copied().collect();
            }
        }
        Ok(())
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
        let Some(rid) = self.rows.get(self.pos) else {
            return Ok(Value::Null);
        };
        let Some(row) = st.rows.get(rid) else {
            return Ok(Value::Null);
        };
        if i == 0 {
            Ok(row.shape.clone())
        } else {
            Ok(row.aux.get(i - 1).cloned().unwrap_or(Value::Null))
        }
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.rows.get(self.pos).copied().unwrap_or(0))
    }
}

/// The geopoly module instance.
pub fn geopoly_module() -> std::sync::Arc<dyn VirtualTableModule> {
    std::sync::Arc::new(GeopolyModule)
}

struct GeopolyModule;

impl VirtualTableModule for GeopolyModule {
    fn name(&self) -> &str {
        "geopoly"
    }

    fn caps(&self) -> u32 {
        ModuleCaps::WRITABLE
    }

    fn create(&self, table: &str, args: &[String]) -> Result<Box<dyn VirtualTable>> {
        Ok(Box::new(GeoTable {
            state: std::sync::Arc::new(parking_lot::Mutex::new(GeoState {
                cfg: GeoConfig {
                    table: table.to_string(),
                    aux_cols: args.to_vec(),
                },
                rows: BTreeMap::new(),
                tree: RTree2D::new(),
            })),
        }))
    }
}
