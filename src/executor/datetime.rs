//! SQLite-compatible date/time engine.
//!
//! This is a faithful port of SQLite's `src/date.c` (as of 3.4x) to Rust:
//! - Internally, every point in time is a Julian Day number scaled by
//!   86400000 (i.e. milliseconds since -4713-11-24 12:00:00 GMT).
//! - The same lazy `validJD/validYMD/validHMS` invalidation scheme is used
//!   so that modifier chains behave bit-identically to SQLite.
//! - Accepts the same timestring grammar (ISO-8601 subsets, Julian day
//!   numbers, unix epochs, `now`) and the same modifiers
//!   (`NNN days`, `start of month`, `weekday N`, `unixepoch`, `localtime`,
//!   `utc`, `subsec`, `ceiling`, `floor`, `end of ...`, `auto`,
//!   `+/-YYYY-MM-DD`, `+/-HH:MM:SS.FFF`).
//! - `strftime` supports every conversion SQLite does (%d %e %f %F %G %g %H
//!   %I %j %J %k %l %m %M %p %P %R %s %S %T %u %U %V %w %W %Y %%).
//!
//! `localtime`/`utc` use a small built-in TZif ( Olson ) database reader
//! backed by `/etc/localtime` and the `TZ` environment variable, so no new
//! crate dependencies are needed and the results match the OS `localtime_r`
//! that SQLite itself uses.

use crate::types::Value;

/// Julian day number of the Unix epoch (1970-01-01 00:00:00), in ms units.
const UNIX_EPOCH_JD_MS: i64 = 210866760_000_000;
/// Maximum valid iJD: 9999-12-31 23:59:59.999
const MAX_JD_MS: i64 = 464_269_060_799_999;

/// A point in time with lazy Y/M/D h:m:s components — mirrors `DateTime`
/// in SQLite's date.c.
#[derive(Clone, Debug)]
pub struct DateTime {
    /// Julian day number * 86400000 (ms).
    pub i_jd: i64,
    pub y: i32,
    pub m: i32,
    pub d: i32,
    pub h: i32,
    pub min: i32,
    pub s: f64,
    /// Timezone offset in minutes (from `+HH:MM` suffixes).
    pub tz: i32,
    pub valid_jd: bool,
    /// `s` holds a raw number (unix epoch or JD) not yet classified.
    pub raw_s: bool,
    pub valid_ymd: bool,
    pub valid_hms: bool,
    pub is_error: bool,
    pub use_subsec: bool,
    pub is_local: bool,
    pub is_utc: bool,
    pub n_floor: i32,
}

impl Default for DateTime {
    fn default() -> Self {
        DateTime {
            i_jd: 0,
            y: 0,
            m: 0,
            d: 0,
            h: 0,
            min: 0,
            s: 0.0,
            tz: 0,
            valid_jd: false,
            raw_s: false,
            valid_ymd: false,
            valid_hms: false,
            is_error: false,
            use_subsec: false,
            is_local: false,
            is_utc: false,
            n_floor: 0,
        }
    }
}

fn datetime_error(p: &mut DateTime) {
    *p = DateTime::default();
    p.is_error = true;
}

fn valid_julian_day(i_jd: i64) -> bool {
    (0..=MAX_JD_MS).contains(&i_jd)
}

/// Current time as iJD (ms), UTC.
fn now_jd_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ms = now.as_millis() as i64;
    UNIX_EPOCH_JD_MS + ms
}

/// Days that need subtracting to roll an overflown day-of-month back to the
/// end of the previous month. Mirrors `computeFloor` in date.c.
fn compute_floor(p: &mut DateTime) {
    if p.d <= 28 {
        p.n_floor = 0;
    } else if (1u32 << p.m) & 0x15aa != 0 {
        // Months with 31 days: 1,3,5,7,8,10,12 (bitmask check from date.c)
        p.n_floor = 0;
    } else if p.m != 2 {
        p.n_floor = if p.d == 31 { 1 } else { 0 };
    } else if p.y % 4 != 0 || (p.y % 100 == 0 && p.y % 400 != 0) {
        p.n_floor = p.d - 28;
    } else {
        p.n_floor = p.d - 29;
    }
}

/// Convert Y/M/D h:m:s (+tz) to Julian day ms. Mirrors `computeJD`.
fn compute_jd(p: &mut DateTime) {
    if p.valid_jd {
        return;
    }
    let (mut y, mut m, d);
    if p.valid_ymd {
        y = p.y;
        m = p.m;
        d = p.d;
    } else {
        y = 2000;
        m = 1;
        d = 1;
    }
    if !(-4713..=9999).contains(&y) || p.raw_s {
        datetime_error(p);
        return;
    }
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let a = (y + 4800) / 100;
    let b = 38 - a + a / 4;
    let x1 = 36525 * (y + 4716) / 100;
    let x2 = 306001 * (m + 1) / 10000;
    p.i_jd = ((x1 + x2 + d + b - 1524) as i64) * 86400000
        - 43200000; // -1524.5 days → subtract half a day
    p.valid_jd = true;
    if p.valid_hms {
        p.i_jd += p.h as i64 * 3600000 + p.min as i64 * 60000 + (p.s * 1000.0 + 0.5) as i64;
        if p.tz != 0 {
            p.i_jd -= p.tz as i64 * 60000;
            p.valid_ymd = false;
            p.valid_hms = false;
            p.tz = 0;
            p.is_utc = true;
            p.is_local = false;
        }
    }
}

/// Compute Y/M/D from iJD. Mirrors `computeYMD`.
fn compute_ymd(p: &mut DateTime) {
    if p.valid_ymd {
        return;
    }
    if !p.valid_jd {
        p.y = 2000;
        p.m = 1;
        p.d = 1;
    } else if !valid_julian_day(p.i_jd) {
        datetime_error(p);
        return;
    } else {
        let z = ((p.i_jd + 43200000) / 86400000) as i64;
        let alpha = ((z as f64 + 32044.75) / 36524.25) as i64 - 52;
        let a = z + 1 + alpha - (alpha + 100) / 4 + 25;
        let b = a + 1524;
        let c = ((b as f64 - 122.1) / 365.25) as i64;
        let d = (36525 * (c & 32767)) / 100;
        let e = ((b - d) as f64 / 30.6001) as i64;
        let x1 = (30.6001 * e as f64) as i64;
        p.d = (b - d - x1) as i32;
        p.m = if e < 14 { (e - 1) as i32 } else { (e - 13) as i32 };
        p.y = if p.m > 2 { (c - 4716) as i32 } else { (c - 4715) as i32 };
    }
    p.valid_ymd = true;
}

/// Compute h/m/s from iJD. Mirrors `computeHMS`.
fn compute_hms(p: &mut DateTime) {
    if p.valid_hms {
        return;
    }
    compute_jd(p);
    let day_ms = ((p.i_jd + 43200000) % 86400000) as i64;
    p.s = (day_ms % 60000) as f64 / 1000.0;
    let day_min = day_ms / 60000;
    p.min = (day_min % 60) as i32;
    p.h = (day_min / 60) as i32;
    p.raw_s = false;
    p.valid_hms = true;
}

fn compute_ymd_hms(p: &mut DateTime) {
    compute_ymd(p);
    compute_hms(p);
}

fn clear_ymd_hms_tz(p: &mut DateTime) {
    p.valid_ymd = false;
    p.valid_hms = false;
    p.tz = 0;
}

/// Read exactly `n` ASCII digits at `z[i..]`, returning None on failure.
fn digits(z: &[u8], i: usize, n: usize) -> Option<u32> {
    if i + n > z.len() {
        return None;
    }
    let mut val: u32 = 0;
    for k in 0..n {
        let c = z[i + k];
        if !c.is_ascii_digit() {
            return None;
        }
        val = val * 10 + (c - b'0') as u32;
    }
    Some(val)
}

/// Parse a timezone suffix `+HH:MM`, `-HH:MM`, or `Z`. Returns (tz minutes, ok).
fn parse_timezone(z: &[u8], p: &mut DateTime) -> bool {
    let mut i = 0;
    while i < z.len() && z[i].is_ascii_whitespace() {
        i += 1;
    }
    p.tz = 0;
    if i >= z.len() {
        return true; // no timezone — not an error
    }
    let c = z[i];
    let sgn: i32 = match c {
        b'-' => -1,
        b'+' => 1,
        b'Z' | b'z' => {
            // zulu
            let mut j = i + 1;
            while j < z.len() && z[j].is_ascii_whitespace() {
                j += 1;
            }
            return j == z.len();
        }
        _ => return false,
    };
    let n_hr = digits(z, i + 1, 2);
    if n_hr.is_none() || i + 3 >= z.len() || z[i + 3] != b':' {
        return false;
    }
    let n_mn = digits(z, i + 4, 2);
    if n_mn.is_none() {
        return false;
    }
    p.tz = sgn * (n_mn.unwrap() as i32 + n_hr.unwrap() as i32 * 60);
    if p.tz == 0 {
        p.is_local = false;
        p.is_utc = true;
    }
    let mut j = i + 6;
    while j < z.len() && z[j].is_ascii_whitespace() {
        j += 1;
    }
    j == z.len()
}

/// Parse `HH:MM`, `HH:MM:SS`, `HH:MM:SS.FFF`. Mirrors `parseHhMmSs`.
fn parse_hhmmss(z: &[u8], p: &mut DateTime) -> bool {
    let h = match digits(z, 0, 2) {
        Some(v) => v as i32,
        None => return false,
    };
    if z.len() < 3 || z[2] != b':' {
        return false;
    }
    let m = match digits(z, 3, 2) {
        Some(v) => v as i32,
        None => return false,
    };
    let mut i = 5;
    let mut s: f64 = 0.0;
    if i < z.len() && z[i] == b':' {
        s = match digits(z, i + 1, 2) {
            Some(v) => v as f64,
            None => return false,
        };
        i += 3;
        if i < z.len() && z[i] == b'.' && i + 1 < z.len() && z[i + 1].is_ascii_digit() {
            i += 1;
            let mut ms = 0.0f64;
            let mut r_scale = 1.0f64;
            while i < z.len() && z[i].is_ascii_digit() {
                ms = ms * 10.0 + (z[i] - b'0') as f64;
                r_scale *= 10.0;
                i += 1;
            }
            ms /= r_scale;
            if ms > 0.999 {
                ms = 0.999;
            }
            s += ms;
        }
    }
    p.valid_jd = false;
    p.raw_s = false;
    p.valid_hms = true;
    p.h = h;
    p.min = m;
    p.s = s;
    if !parse_timezone(&z[i..], p) {
        return false;
    }
    true
}

/// Parse `YYYY-MM-DD[ HH:MM:SS.FFF][tz]` and `T` separator.
/// Mirrors `parseYyyyMmDd`.
fn parse_ymd(z: &[u8], p: &mut DateTime) -> bool {
    let mut i = 0;
    let neg = !z.is_empty() && z[0] == b'-';
    if neg {
        i = 1;
    }
    let y = match digits(z, i, 4) {
        Some(v) => v as i32,
        None => return false,
    };
    if i + 4 >= z.len() || z[i + 4] != b'-' {
        return false;
    }
    let m = match digits(z, i + 5, 2) {
        Some(v) => v as i32,
        None => return false,
    };
    if i + 7 >= z.len() || z[i + 7] != b'-' {
        return false;
    }
    let d = match digits(z, i + 8, 2) {
        Some(v) => v as i32,
        None => return false,
    };
    let mut j = i + 10;
    while j < z.len() && (z[j].is_ascii_whitespace() || z[j] == b'T' || z[j] == b't') {
        j += 1;
    }
    if j < z.len() {
        if !parse_hhmmss(&z[j..], p) {
            return false;
        }
    } else {
        p.valid_hms = false;
    }
    p.valid_jd = false;
    p.valid_ymd = true;
    p.y = if neg { -y } else { y };
    p.m = m;
    p.d = d;
    compute_floor(p);
    if p.tz != 0 {
        compute_jd(p);
    }
    true
}

/// Install a raw number as Julian day if in range. Mirrors `setRawDateNumber`.
fn set_raw_date_number(p: &mut DateTime, r: f64) {
    p.s = r;
    p.raw_s = true;
    if (0.0..5373484.5).contains(&r) {
        p.i_jd = (r * 86400000.0 + 0.5) as i64;
        p.valid_jd = true;
    }
}

/// If p holds a raw number, classify it as JD or unix epoch.
/// Mirrors `autoAdjustDate`.
fn auto_adjust_date(p: &mut DateTime) {
    if !p.raw_s || p.valid_jd {
        p.raw_s = false;
    } else if p.s >= -21086676.0 * 10000.0 && p.s <= 25340230.0 * 10000.0 + 799.0 {
        let r = p.s * 1000.0 + 210866760000000.0;
        clear_ymd_hms_tz(p);
        p.i_jd = (r + 0.5) as i64;
        p.valid_jd = true;
        p.raw_s = false;
    }
}

/// Try to parse a string as a float (like sqlite3AtoF — accepts leading/trailing
/// spaces, signs, decimals, exponents).
fn atof(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    // Reject strings with embedded spaces (sqlite3AtoF would fail on those).
    if t.contains(' ') {
        return None;
    }
    t.parse::<f64>().ok()
}

/// Parse a timestring (first argument). Mirrors `parseDateOrTime`.
fn parse_date_or_time(z: &str, p: &mut DateTime) -> bool {
    if parse_ymd(z.as_bytes(), p) {
        return true;
    }
    if parse_hhmmss(z.as_bytes(), p) {
        return true;
    }
    if z.eq_ignore_ascii_case("now") {
        p.i_jd = now_jd_ms();
        p.valid_jd = true;
        p.is_utc = true;
        p.is_local = false;
        clear_ymd_hms_tz(p);
        return true;
    }
    if let Some(r) = atof(z) {
        set_raw_date_number(p, r);
        return true;
    }
    if z.eq_ignore_ascii_case("subsec") || z.eq_ignore_ascii_case("subsecond") {
        p.use_subsec = true;
        p.i_jd = now_jd_ms();
        p.valid_jd = true;
        p.is_utc = true;
        clear_ymd_hms_tz(p);
        return true;
    }
    false
}

/// The transformation units table. Mirrors `aXformType` in date.c.
const XFORM_TYPES: [(u8, &str, f32, f64); 6] = [
    (6, "second", 4.6427e14, 1.0),
    (6, "minute", 7.7379e12, 60.0),
    (4, "hour", 1.2897e11, 3600.0),
    (3, "day", 5373485.0, 86400.0),
    (5, "month", 176546.0, 2592000.0),
    (4, "year", 14713.0, 31536000.0),
];

/// Process one modifier. Mirrors `parseModifier` (returns true on success).
fn parse_modifier(z: &str, idx: usize, p: &mut DateTime) -> bool {
    let z = z.trim();
    if z.is_empty() {
        return false;
    }
    let first = z.as_bytes()[0].to_ascii_lowercase();
    match first {
        b'a' => {
            // auto
            if z.eq_ignore_ascii_case("auto") {
                if idx > 1 {
                    return false;
                }
                auto_adjust_date(p);
                return true;
            }
            false
        }
        b'c' => {
            // ceiling — default behavior, no-op
            if z.eq_ignore_ascii_case("ceiling") {
                compute_jd(p);
                clear_ymd_hms_tz(p);
                p.n_floor = 0;
                return true;
            }
            false
        }
        b'e' => {
            // end of day / end of month / end of year
            if !z.to_ascii_lowercase().starts_with("end of ") {
                return false;
            }
            if !p.valid_jd && !p.valid_ymd && !p.valid_hms {
                return false;
            }
            let unit = &z[7..];
            compute_ymd(p);
            p.valid_hms = true;
            p.h = 23;
            p.min = 59;
            p.s = 59.999;
            p.raw_s = false;
            p.tz = 0;
            p.valid_jd = false;
            if unit.eq_ignore_ascii_case("month") {
                p.d = 1;
                p.m += 1;
                if p.m > 12 {
                    p.y += 1;
                    p.m = 1;
                }
                compute_floor(p);
                compute_jd(p);
                p.i_jd -= (p.n_floor as i64 + 1) * 86400000;
                clear_ymd_hms_tz(p);
                true
            } else if unit.eq_ignore_ascii_case("year") {
                p.m = 12;
                p.d = 31;
                true
            } else if unit.eq_ignore_ascii_case("day") {
                true
            } else {
                false
            }
        }
        b'f' => {
            // floor
            if z.eq_ignore_ascii_case("floor") {
                compute_jd(p);
                p.i_jd -= p.n_floor as i64 * 86400000;
                clear_ymd_hms_tz(p);
                return true;
            }
            false
        }
        b'j' => {
            // julianday
            if z.eq_ignore_ascii_case("julianday") {
                if idx > 1 {
                    return false;
                }
                if p.valid_jd && p.raw_s {
                    p.raw_s = false;
                    return true;
                }
            }
            false
        }
        b'l' => {
            // localtime
            if z.eq_ignore_ascii_case("localtime") {
                if !p.is_local {
                    to_localtime(p);
                }
                p.is_utc = false;
                p.is_local = true;
                return true;
            }
            false
        }
        b'u' => {
            if z.eq_ignore_ascii_case("unixepoch") && p.raw_s {
                if idx > 1 {
                    return false;
                }
                let r = p.s * 1000.0 + 210866760000000.0;
                if (0.0..464269060800000.0).contains(&r) {
                    clear_ymd_hms_tz(p);
                    p.i_jd = (r + 0.5) as i64;
                    p.valid_jd = true;
                    p.raw_s = false;
                    return true;
                }
                false
            } else if z.eq_ignore_ascii_case("utc") {
                if !p.is_utc {
                    let i_orig_jd;
                    let mut i_guess;
                    let mut cnt = 0;
                    compute_jd(p);
                    i_orig_jd = p.i_jd;
                    i_guess = i_orig_jd;
                    let mut i_err = 0i64;
                    loop {
                        let mut new_dt = DateTime::default();
                        i_guess -= i_err;
                        new_dt.i_jd = i_guess;
                        new_dt.valid_jd = true;
                        to_localtime(&mut new_dt);
                        compute_jd(&mut new_dt);
                        i_err = new_dt.i_jd.wrapping_sub(i_orig_jd);
                        if i_err == 0 || cnt >= 3 {
                            break;
                        }
                        cnt += 1;
                    }
                    *p = DateTime::default();
                    p.i_jd = i_guess;
                    p.valid_jd = true;
                    p.is_utc = true;
                    p.is_local = false;
                }
                true
            } else {
                false
            }
        }
        b'w' => {
            // weekday N
            let lower = z.to_ascii_lowercase();
            if !lower.starts_with("weekday ") {
                return false;
            }
            let num_part = &z[8..];
            let r = match atof(num_part) {
                Some(v) => v,
                None => return false,
            };
            if !(-6.0..=6.0).contains(&r) || r.fract() != 0.0 {
                return false;
            }
            let mut n = r as i32;
            compute_ymd_hms(p);
            p.tz = 0;
            p.valid_jd = false;
            compute_jd(p);
            let mut wd = ((p.i_jd + 129600000) / 86400000 % 7) as i32;
            if n < 0 {
                n = -n;
            }
            if wd != n {
                if wd > n {
                    wd -= 7;
                }
                p.i_jd += (n - wd) as i64 * 86400000;
                if num_part.contains('-') {
                    p.i_jd -= 7 * 86400000;
                }
            }
            clear_ymd_hms_tz(p);
            true
        }
        b's' => {
            let lower = z.to_ascii_lowercase();
            if !lower.starts_with("start of ") {
                if z.eq_ignore_ascii_case("subsec") || z.eq_ignore_ascii_case("subsecond") {
                    p.use_subsec = true;
                    return true;
                }
                return false;
            }
            if !p.valid_jd && !p.valid_ymd && !p.valid_hms {
                return false;
            }
            let unit = &z[9..];
            compute_ymd(p);
            p.valid_hms = true;
            p.h = 0;
            p.min = 0;
            p.s = 0.0;
            p.raw_s = false;
            p.tz = 0;
            p.valid_jd = false;
            if unit.eq_ignore_ascii_case("month") {
                p.d = 1;
                true
            } else if unit.eq_ignore_ascii_case("year") {
                p.m = 1;
                p.d = 1;
                true
            } else if unit.eq_ignore_ascii_case("day") {
                true
            } else if unit.eq_ignore_ascii_case("week") {
                // Not in older SQLite; treat as unsupported
                false
            } else {
                false
            }
        }
        b'+' | b'-' | b'0'..=b'9' => parse_numeric_modifier(z, p),
        _ => false,
    }
}

/// Handle `+NNN days` style, `+HH:MM:SS` style, and `+YYYY-MM-DD` style
/// modifiers. Mirrors the numeric case of `parseModifier`.
fn parse_numeric_modifier(z: &str, p: &mut DateTime) -> bool {
    // Find the end of the numeric part.
    let bytes = z.as_bytes();
    let mut n = 0usize;
    let z0 = bytes[0];
    while n < bytes.len() {
        let c = bytes[n];
        if c == b':' || c.is_ascii_whitespace() {
            break;
        }
        if c == b'-' {
            if n == 5 && digits(&bytes[1..], 0, 4).is_some() {
                break;
            }
            if n == 6 && digits(&bytes[1..], 0, 5).is_some() {
                break;
            }
        }
        n += 1;
    }
    let num_str = &z[..n];
    let r = match atof(num_str) {
        Some(v) => v,
        None => return false,
    };

    // (+|-)YYYY-MM-DD [HH:MM]
    if n < bytes.len() && bytes[n] == b'-' {
        if z0 != b'+' && z0 != b'-' {
            return false;
        }
        // Parse Y-M-D from the numeric part + following text.
        let full = &z[1..]; // skip sign
        let (y, m, d) = if num_str.len() == 5 {
            // -YYYY from num_str, then -MM-DD from the rest
            match split_ymd(&z[1..], 4) {
                Some(t) => t,
                None => return false,
            }
        } else {
            match split_ymd(&z[1..], 5) {
                Some(t) => t,
                None => return false,
            }
        };
        let (y, m, d) = (y as i32, m as i32, d as i32);
        if m >= 12 || d >= 31 {
            return false;
        }
        compute_ymd_hms(p);
        p.valid_jd = false;
        let mut d = d;
        if z0 == b'-' {
            p.y -= y;
            p.m -= m;
            d = -d;
        } else {
            p.y += y;
            p.m += m;
        }
        let x = if p.m > 0 { (p.m - 1) / 12 } else { (p.m - 12) / 12 };
        p.y += x;
        p.m -= x * 12;
        compute_floor(p);
        compute_jd(p);
        p.valid_hms = false;
        p.valid_ymd = false;
        p.i_jd += d as i64 * 86400000;
        // Optional HH:MM tail
        let tail_start = 1 + if num_str.len() == 5 { 10 } else { 11 };
        if z.len() <= tail_start {
            return true;
        }
        let tail = &z[tail_start..];
        let tail_bytes = tail.as_bytes();
        if tail_bytes[0].is_ascii_whitespace() {
            if let Some(hhmm) = parse_hhmm_value(&tail[1..]) {
                apply_hhmm_shift(p, hhmm, z0 == b'-');
                return true;
            }
        }
        return false;
    }

    // (+|-)HH:MM:SS.FFF
    if n < bytes.len() && bytes[n] == b':' {
        let mut start = 0usize;
        if !bytes[0].is_ascii_digit() {
            start = 1; // skip sign
        }
        // Build a "HH:MM:SS" string to parse with parse_hhmmss.
        let tail = &z[start..];
        let mut tx = DateTime::default();
        if !parse_hhmmss(tail.as_bytes(), &mut tx) {
            return false;
        }
        compute_jd(&mut tx);
        tx.i_jd -= 43200000;
        let day = tx.i_jd / 86400000;
        tx.i_jd -= day * 86400000;
        if z0 == b'-' {
            tx.i_jd = -tx.i_jd;
        }
        compute_jd(p);
        clear_ymd_hms_tz(p);
        p.i_jd += tx.i_jd;
        return true;
    }

    // `+NNN unit` form
    let unit = z[n..].trim_start();
    let mut unit = unit;
    let mut ulen = unit.len();
    if ulen < 3 || ulen > 10 {
        return false;
    }
    if unit.as_bytes()[ulen - 1].to_ascii_lowercase() == b's' {
        ulen -= 1;
        unit = &unit[..ulen];
    }
    compute_jd(p);
    let r_rounder = if r < 0.0 { -0.5 } else { 0.5 };
    p.n_floor = 0;
    for (i, &(nlen, name, r_limit, r_xform)) in XFORM_TYPES.iter().enumerate() {
        if ulen == nlen as usize
            && unit.eq_ignore_ascii_case(name)
            && r > -(r_limit as f64)
            && r < r_limit as f64
        {
            let mut r = r;
            match i {
                4 => {
                    // months
                    compute_ymd_hms(p);
                    p.m += r as i32;
                    let x = if p.m > 0 { (p.m - 1) / 12 } else { (p.m - 12) / 12 };
                    p.y += x;
                    p.m -= x * 12;
                    compute_floor(p);
                    p.valid_jd = false;
                    r -= r as i32 as f64;
                }
                5 => {
                    // years
                    let y = r as i32;
                    compute_ymd_hms(p);
                    p.y += y;
                    compute_floor(p);
                    p.valid_jd = false;
                    r -= r as i32 as f64;
                }
                _ => {}
            }
            compute_jd(p);
            p.i_jd += (r * 1000.0 * r_xform + r_rounder) as i64;
            clear_ymd_hms_tz(p);
            return true;
        }
    }
    false
}

/// Split `YYYY-MM-DD` from the start of s where the year is `ylen` digits.
fn split_ymd(s: &str, ylen: usize) -> Option<(u32, u32, u32)> {
    let b = s.as_bytes();
    let y = digits(b, 0, ylen)?;
    if b.get(ylen) != Some(&b'-') {
        return None;
    }
    let m = digits(b, ylen + 1, 2)?;
    if b.get(ylen + 3) != Some(&b'-') {
        return None;
    }
    let d = digits(b, ylen + 4, 2)?;
    Some((y, m, d))
}

/// Parse `HH:MM` for the tail of a `+YYYY-MM-DD HH:MM` modifier.
fn parse_hhmm_value(s: &str) -> Option<(i32, i32)> {
    let b = s.as_bytes();
    let h = digits(b, 0, 2)? as i32;
    if b.get(2) != Some(&b':') {
        return None;
    }
    let m = digits(b, 3, 2)? as i32;
    Some((h, m))
}

fn apply_hhmm_shift(p: &mut DateTime, hhmm: (i32, i32), negative: bool) {
    let (h, m) = hhmm;
    let mut ms = (h as i64) * 3600000 + (m as i64) * 60000;
    if negative {
        ms = -ms;
    }
    compute_jd(p);
    clear_ymd_hms_tz(p);
    p.i_jd += ms;
}

/// Shift a UTC DateTime to local time using the OS timezone database.
/// Mirrors `toLocaltime` (without the year-folding trick, which only matters
/// for 1970..2038 range checks in C `localtime_r`).
fn to_localtime(p: &mut DateTime) {
    compute_jd(p);
    let t = p.i_jd / 1000 - 210866760_000 / 1000 * 1000;
    // t should be unix seconds: iJD/1000 - 21086676*10000
    let t = p.i_jd / 1000 - 21086676 * 10000;
    let _ = t;
    let unix_secs = p.i_jd / 1000 - 21086676 * 10000;
    if let Some(offset_secs) = tz_offset_for_unix(unix_secs) {
        let mut shifted = p.clone();
        shifted.i_jd += offset_secs as i64 * 1000;
        // Extract Y/M/D h:m:s from the shifted JD.
        compute_ymd(&mut DateTime { valid_ymd: false, ..shifted.clone() });
        let mut tmp = shifted;
        tmp.valid_ymd = false;
        tmp.valid_hms = false;
        compute_ymd_hms(&mut tmp);
        p.y = tmp.y;
        p.m = tmp.m;
        p.d = tmp.d;
        p.h = tmp.h;
        p.min = tmp.min;
        p.s = tmp.s;
        p.valid_ymd = true;
        p.valid_hms = true;
        p.valid_jd = false;
        p.raw_s = false;
        p.tz = 0;
        p.is_error = false;
    }
}

// ============================================================================
// TZif (Olson database) reader — minimal, dependency-free
// ============================================================================

/// Get the UTC offset (seconds east of UTC) in effect at the given unix time,
/// according to the system timezone. Falls back to +0 (UTC).
fn tz_offset_for_unix(unix_secs: i64) -> Option<i32> {
    static CACHE: std::sync::OnceLock<TzData> = std::sync::OnceLock::new();
    let tz = CACHE.get_or_init(load_system_tz);
    tz.offset_at(unix_secs)
}

/// The system timezone, parsed once and cached.
struct TzData {
    /// TZif transitions (may be empty → fixed UTC).
    tzif: Option<TzTransition>,
}

impl TzData {
    fn offset_at(&self, t: i64) -> Option<i32> {
        match &self.tzif {
            Some(tz) => tz.offset_at(t),
            None => None,
        }
    }
}

struct TzTransition {
    /// Transition times (unix seconds), sorted ascending.
    times: Vec<i64>,
    /// Index into `offsets` for each transition.
    idx: Vec<u8>,
    /// UTC offsets (seconds east) by type.
    offsets: Vec<i32>,
    /// Whether each type is DST.
    isdst: Vec<bool>,
}

impl TzTransition {
    /// Offset in effect at the given time. Type 0 is used before the first
    /// transition; the last applicable transition otherwise.
    fn offset_at(&self, t: i64) -> Option<i32> {
        if self.offsets.is_empty() {
            return None;
        }
        if self.times.is_empty() {
            // Pick the first non-DST type if available (standard time).
            let i = self.isdst.iter().position(|&d| !d).unwrap_or(0);
            return Some(self.offsets[i]);
        }
        if t < self.times[0] {
            // Before the first transition: first standard type, else type 0.
            let i = self.isdst.iter().position(|&d| !d).unwrap_or(0);
            return Some(self.offsets[i]);
        }
        // Binary search for the last transition <= t.
        let mut lo = 0usize;
        let mut hi = self.times.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.times[mid] <= t {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let i = self.idx[lo - 1] as usize;
        self.offsets.get(i).copied()
    }
}

/// Load the system timezone. Prefers `TZ` env var (POSIX paths like
/// `:/etc/localtime`, `Asia/Tokyo`, or `UTC`), then `/etc/localtime`.
fn load_system_tz() -> TzData {
    if let Ok(tz) = std::env::var("TZ") {
        if tz.is_empty() || tz == "UTC" || tz == "UTC0" || tz == ":UTC" {
            return TzData { tzif: None };
        }
        let path = if let Some(stripped) = tz.strip_prefix(':') {
            Some(stripped.to_string())
        } else if tz.contains('/') || tz.starts_with('.') {
            // A path or a zone name.
            if std::path::Path::new(&tz).exists() {
                Some(tz.clone())
            } else {
                Some(format!("/usr/share/zoneinfo/{}", tz))
            }
        } else {
            Some(format!("/usr/share/zoneinfo/{}", tz))
        };
        if let Some(p) = path {
            if let Some(t) = parse_tzif_file(&p) {
                return TzData { tzif: Some(t) };
            }
        }
        // Unparseable TZ (e.g. POSIX string "CST-6"): fall through to localtime.
    }
    TzData {
        tzif: parse_tzif_file("/etc/localtime"),
    }
}

/// Parse a TZif file (v1 or v2/v3) into transitions.
fn parse_tzif_file(path: &str) -> Option<TzTransition> {
    let data = std::fs::read(path).ok()?;
    parse_tzif(&data)
}

fn parse_tzif(data: &[u8]) -> Option<TzTransition> {
    if data.len() < 44 || &data[0..4] != b"TZif" {
        return None;
    }
    let version = data[4];
    let mut times = Vec::new();
    let mut idx = Vec::new();
    let mut offsets = Vec::new();
    let mut isdst = Vec::new();

    let mut pos = parse_tzif_block(data, 20, 4, &mut times, &mut idx, &mut offsets, &mut isdst)?;
    if version >= b'2' {
        // Skip the v1 block, parse the 64-bit block.
        times.clear();
        idx.clear();
        offsets.clear();
        isdst.clear();
        // The second header starts right after the v1 data block.
        let hdr2 = pos as usize;
        if data.len() < hdr2 + 44 || &data[hdr2..hdr2 + 4] != b"TZif" {
            return None;
        }
        pos = parse_tzif_block(data, hdr2 + 20, 8, &mut times, &mut idx, &mut offsets, &mut isdst)?;
        let _ = 0usize;
    }
    Some(TzTransition { times, idx, offsets, isdst })
}

/// Parse one TZif data block starting at header offset `hdr_off` with
/// `time_size`-byte transition times. Returns the end offset.
#[allow(clippy::too_many_arguments)]
fn parse_tzif_block(
    data: &[u8],
    hdr_off: usize,
    time_size: usize,
    times: &mut Vec<i64>,
    idx: &mut Vec<u8>,
    offsets: &mut Vec<i32>,
    isdst: &mut Vec<bool>,
) -> Option<u64> {
    if data.len() < hdr_off + 24 {
        return None;
    }
    let rd_u32 = |o: usize| -> u32 {
        u32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
    };
    let _isutcnt = rd_u32(hdr_off);
    let _isstdcnt = rd_u32(hdr_off + 4);
    let leapcnt = rd_u32(hdr_off + 8);
    let timecnt = rd_u32(hdr_off + 12) as usize;
    let typecnt = rd_u32(hdr_off + 16) as usize;
    let charcnt = rd_u32(hdr_off + 20) as usize;
    let mut pos = hdr_off + 24;
    // Transition times.
    for _ in 0..timecnt {
        if pos + time_size > data.len() {
            return None;
        }
        let mut v: i64 = if time_size == 4 {
            i32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as i64
        } else {
            i64::from_be_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
            ])
        };
        if time_size == 4 {
            // sign-extend already done via i32 cast
        }
        v = v; // silence unused when time_size==8 path reassigns
        times.push(v);
        pos += time_size;
    }
    // Type indices.
    for _ in 0..timecnt {
        if pos >= data.len() {
            return None;
        }
        idx.push(data[pos]);
        pos += 1;
    }
    // Type records (ttinfo): 4-byte gmtoff, 1-byte isdst, 1-byte abbrind.
    for _ in 0..typecnt {
        if pos + 6 > data.len() {
            return None;
        }
        let off = i32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        offsets.push(off);
        isdst.push(data[pos + 4] != 0);
        pos += 6;
    }
    // Timezone abbreviation bytes.
    pos += charcnt;
    // Leap seconds (each 4+4 or 8+4 bytes).
    pos += leapcnt as usize * (time_size + 4);
    // isstd + isut arrays.
    pos += _isstdcnt as usize + _isutcnt as usize;
    Some(pos as u64)
}

// ============================================================================
// Public SQL function implementations
// ============================================================================

/// Parse `args` (timestring + modifiers) into a normalized DateTime.
/// Mirrors `isDate`. Returns None on error → SQL NULL.
fn is_date(args: &[Value]) -> Option<DateTime> {
    let mut p = DateTime::default();
    if args.is_empty() {
        p.i_jd = now_jd_ms();
        p.valid_jd = true;
        p.is_utc = true;
        return Some(p);
    }
    // The timestring.
    match &args[0] {
        Value::Integer(i) => set_raw_date_number(&mut p, *i as f64),
        Value::Real(f) => set_raw_date_number(&mut p, *f),
        v => {
            let s = v.as_text();
            if s.is_empty() && v.is_null() {
                return None;
            }
            if !parse_date_or_time(&s, &mut p) {
                return None;
            }
        }
    }
    // Modifiers.
    for (i, m) in args.iter().enumerate().skip(1) {
        if m.is_null() {
            return None;
        }
        let s = m.as_text();
        if !parse_modifier(&s, i, &mut p) {
            return None;
        }
    }
    compute_jd(&mut p);
    if p.is_error || !valid_julian_day(p.i_jd) {
        return None;
    }
    if args.len() == 1 && p.valid_ymd && p.d > 28 {
        // Normalize dates like 2023-02-31 → 2023-03-03.
        p.valid_ymd = false;
    }
    Some(p)
}

/// `date(...)` → `YYYY-MM-DD`
pub fn date_func(args: &[Value]) -> Option<Value> {
    let mut x = is_date(args)?;
    compute_ymd(&mut x);
    if x.is_error {
        return None;
    }
    let y = x.y.abs();
    if x.y < 0 {
        Some(Value::Text(format!("-{:04}-{:02}-{:02}", y, x.m, x.d)))
    } else {
        Some(Value::Text(format!("{:04}-{:02}-{:02}", y, x.m, x.d)))
    }
}

/// `time(...)` → `HH:MM:SS`
pub fn time_func(args: &[Value]) -> Option<Value> {
    let mut x = is_date(args)?;
    compute_hms(&mut x);
    if x.is_error {
        return None;
    }
    if x.use_subsec {
        let s = (1000.0 * x.s + 0.5) as i64;
        Some(Value::Text(format!(
            "{:02}:{:02}:{:02}.{:03}",
            x.h,
            x.min,
            s / 1000,
            s % 1000
        )))
    } else {
        Some(Value::Text(format!("{:02}:{:02}:{:02}", x.h, x.min, x.s as i64)))
    }
}

/// `datetime(...)` → `YYYY-MM-DD HH:MM:SS`
pub fn datetime_func(args: &[Value]) -> Option<Value> {
    let mut x = is_date(args)?;
    compute_ymd_hms(&mut x);
    if x.is_error {
        return None;
    }
    let y = x.y.abs();
    if x.use_subsec {
        let s = (1000.0 * x.s + 0.5) as i64;
        if x.y < 0 {
            Some(Value::Text(format!(
                "-{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                y, x.m, x.d, x.h, x.min, s / 1000, s % 1000
            )))
        } else {
            Some(Value::Text(format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
                y, x.m, x.d, x.h, x.min, s / 1000, s % 1000
            )))
        }
    } else if x.y < 0 {
        Some(Value::Text(format!(
            "-{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            y, x.m, x.d, x.h, x.min, x.s as i64
        )))
    } else {
        Some(Value::Text(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            y, x.m, x.d, x.h, x.min, x.s as i64
        )))
    }
}

/// `julianday(...)`
pub fn julianday_func(args: &[Value]) -> Option<Value> {
    let mut x = is_date(args)?;
    compute_jd(&mut x);
    if x.is_error {
        return None;
    }
    Some(Value::Real(x.i_jd as f64 / 86400000.0))
}

/// `unixepoch(...)`
pub fn unixepoch_func(args: &[Value]) -> Option<Value> {
    let mut x = is_date(args)?;
    compute_jd(&mut x);
    if x.is_error {
        return None;
    }
    if x.use_subsec {
        Some(Value::Real((x.i_jd - 21086676 * 10000000) as f64 / 1000.0))
    } else {
        Some(Value::Integer(x.i_jd / 1000 - 21086676 * 10000))
    }
}

/// `timediff(A, B)` — the exact interval from B to A, rendered as
/// `+YYYY-MM-DD HH:MM:SS.SSS` (SQLite 3.35+ format).
pub fn timediff_func(args: &[Value]) -> Option<Value> {
    if args.len() != 2 {
        return None;
    }
    let a = is_date(&args[..1])?;
    let b = is_date(&args[1..2])?;
    let mut x = DateTime::default();
    // Compute A - B in ms, then break into Y/M/D HH:MM:SS.SSS using
    // SQLite's algorithm (see timediffFunc in date.c).
    let diff_ms = a.i_jd - b.i_jd;
    let neg = diff_ms < 0;
    let mut ms = diff_ms.abs();
    // Days part.
    let days = ms / 86400000;
    ms -= days * 86400000;
    let secs_of_day = ms / 1000;
    let ms_frac = ms % 1000;
    // Convert days into Y/M/D with month arithmetic.
    x.i_jd = 0; // 2000-01-01 12:00? No — compute from epoch -4713-11-24 12:00
    // Simply: start at julian day 0 (i.e. -4713-11-24 12:00) and add `days`.
    let mut dt = DateTime::default();
    dt.i_jd = days * 86400000;
    dt.valid_jd = true;
    compute_ymd(&mut dt);
    // Adjust: JD 0 is -4713-11-24 12:00; the Y/M/D from computeYMD at
    // iJD=0 is -4713-11-24 (with the 12:00 offset ignored).
    // SQLite renders relative to -4713-11-24 12:00 by subtracting the base.
    let (mut y, mut m, d) = (dt.y + 4713, dt.m - 11, dt.d - 24);
    if m <= 0 {
        m += 12;
        y -= 1;
    }
    // Normalize overflow the same way SQLite does (ceil by default).
    compute_floor(&mut dt);
    let h = secs_of_day / 3600;
    let min = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let sign = if neg { "-" } else { "+" };
    Some(Value::Text(format!(
        "{}{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        sign, y, m, d, h, min, s, ms_frac
    )))
}

/// Day number after the most recent Jan 1 (0-based).
fn days_after_jan01(p: &DateTime) -> i64 {
    let mut jan01 = p.clone();
    jan01.valid_jd = false;
    jan01.m = 1;
    jan01.d = 1;
    compute_jd(&mut jan01);
    (p.i_jd - jan01.i_jd + 43200000) / 86400000
}

/// 0=Sunday..6=Saturday
fn days_after_sunday(p: &DateTime) -> i64 {
    (p.i_jd + 129600000) / 86400000 % 7
}

/// 0=Monday..6=Sunday
fn days_after_monday(p: &DateTime) -> i64 {
    (p.i_jd + 43200000) / 86400000 % 7
}

/// `strftime(FORMAT, TIMESTRING, ...)` — SQLite conversion specifiers.
pub fn strftime_func(args: &[Value]) -> Option<Value> {
    if args.is_empty() {
        return None;
    }
    let fmt = args[0].as_text();
    if args[0].is_null() {
        return None;
    }
    let mut x = is_date(&args[1..])?;
    compute_jd(&mut x);
    compute_ymd_hms(&mut x);
    if x.is_error {
        return None;
    }

    let fmt_bytes: Vec<char> = fmt.chars().collect();
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut i = 0usize;
    while i < fmt_bytes.len() {
        if fmt_bytes[i] != '%' {
            out.push(fmt_bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt_bytes.len() {
            // Trailing % — SQLite emits nothing (format invalid → NULL).
            return None;
        }
        let cf = fmt_bytes[i];
        i += 1;
        match cf {
            'd' => out.push_str(&format!("{:02}", x.d)),
            'e' => out.push_str(&format!("{:2}", x.d)),
            'f' => {
                let mut s = x.s;
                if s > 59.999 {
                    s = 59.999;
                }
                out.push_str(&format!("{:06.3}", s));
            }
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", x.y, x.m, x.d)),
            'G' | 'g' => {
                let mut y = x.clone();
                y.i_jd += (3 - days_after_monday(&x)) * 86400000;
                y.valid_ymd = false;
                compute_ymd(&mut y);
                if cf == 'g' {
                    out.push_str(&format!("{:02}", y.y % 100));
                } else {
                    out.push_str(&format!("{:04}", y.y));
                }
            }
            'H' => out.push_str(&format!("{:02}", x.h)),
            'k' => out.push_str(&format!("{:2}", x.h)),
            'I' | 'l' => {
                let mut h = x.h;
                if h > 12 {
                    h -= 12;
                }
                if h == 0 {
                    h = 12;
                }
                if cf == 'I' {
                    out.push_str(&format!("{:02}", h));
                } else {
                    out.push_str(&format!("{:2}", h));
                }
            }
            'j' => out.push_str(&format!("{:03}", days_after_jan01(&x) + 1)),
            'J' => out.push_str(&format!("{:.16}", x.i_jd as f64 / 86400000.0)),
            'm' => out.push_str(&format!("{:02}", x.m)),
            'M' => out.push_str(&format!("{:02}", x.min)),
            'p' | 'P' => {
                let s = if x.h >= 12 {
                    if cf == 'p' { "PM" } else { "pm" }
                } else if cf == 'p' {
                    "AM"
                } else {
                    "am"
                };
                out.push_str(s);
            }
            'R' => out.push_str(&format!("{:02}:{:02}", x.h, x.min)),
            's' => {
                if x.use_subsec {
                    out.push_str(&format!(
                        "{:.3}",
                        (x.i_jd - 21086676 * 10000000) as f64 / 1000.0
                    ));
                } else {
                    let i_s = x.i_jd / 1000 - 21086676 * 10000;
                    out.push_str(&i_s.to_string());
                }
            }
            'S' => out.push_str(&format!("{:02}", x.s as i64)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", x.h, x.min, x.s as i64)),
            'u' | 'w' => {
                let wd = days_after_sunday(&x);
                let c = if wd == 0 && cf == 'u' { 7 } else { wd };
                out.push_str(&c.to_string());
            }
            'U' => out.push_str(&format!(
                "{:02}",
                (days_after_jan01(&x) - days_after_sunday(&x) + 7) / 7
            )),
            'V' => {
                let mut y = x.clone();
                y.i_jd += (3 - days_after_monday(&x)) * 86400000;
                y.valid_ymd = false;
                compute_ymd(&mut y);
                out.push_str(&format!("{:02}", days_after_jan01(&y) / 7 + 1));
            }
            'W' => out.push_str(&format!(
                "{:02}",
                (days_after_jan01(&x) - days_after_monday(&x) + 7) / 7
            )),
            'Y' => out.push_str(&format!("{:04}", x.y)),
            '%' => out.push('%'),
            _ => return None, // unknown conversion → NULL
        }
    }
    Some(Value::Text(out))
}

/// Dispatch a date/time SQL function. Returns `Value::Null` on bad input
/// (matching SQLite, which yields NULL rather than an error).
pub fn call_datetime_function(name: &str, args: &[Value]) -> Value {
    let lname = name.to_ascii_lowercase();
    match lname.as_str() {
        "date" => date_func(args).unwrap_or(Value::Null),
        "time" => time_func(args).unwrap_or(Value::Null),
        "datetime" => datetime_func(args).unwrap_or(Value::Null),
        "julianday" => julianday_func(args).unwrap_or(Value::Null),
        "unixepoch" => unixepoch_func(args).unwrap_or(Value::Null),
        "strftime" => strftime_func(args).unwrap_or(Value::Null),
        "timediff" => timediff_func(args).unwrap_or(Value::Null),
        "current_date" | "currentdate" => date_func(&[]).unwrap_or(Value::Null),
        "current_time" | "currenttime" => time_func(&[]).unwrap_or(Value::Null),
        "current_timestamp" | "currenttimestamp" => {
            datetime_func(&[]).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(v: Option<Value>) -> String {
        match v {
            Some(Value::Text(s)) => s,
            other => format!("{:?}", other),
        }
    }

    #[test]
    fn iso_dates() {
        assert_eq!(text(date_func(&[Value::Text("2023-07-14".into())])), "2023-07-14");
        assert_eq!(
            text(datetime_func(&[Value::Text("2023-07-14 13:45:28".into())])),
            "2023-07-14 13:45:28"
        );
        assert_eq!(
            text(datetime_func(&[Value::Text("2023-07-14T13:45:28".into())])),
            "2023-07-14 13:45:28"
        );
        // Feb 31 normalizes to Mar 3
        assert_eq!(text(date_func(&[Value::Text("2023-02-31".into())])), "2023-03-03");
    }

    #[test]
    fn julian_day_round_trip() {
        // 1970-01-01 00:00:00 = JD 2440587.5
        let jd = julianday_func(&[Value::Text("1970-01-01 00:00:00".into())]);
        match jd {
            Some(Value::Real(f)) => assert!((f - 2440587.5).abs() < 1e-6),
            other => panic!("bad julianday: {:?}", other),
        }
        // unixepoch
        let ue = unixepoch_func(&[Value::Text("1970-01-01 00:00:00".into())]);
        assert_eq!(ue, Some(Value::Integer(0)));
        let ue = unixepoch_func(&[Value::Text("2023-01-01 00:00:00".into())]);
        assert_eq!(ue, Some(Value::Integer(1672531200)));
    }

    #[test]
    fn modifiers() {
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-01-31".into()),
                Value::Text("+1 day".into())
            ])),
            "2023-02-01"
        );
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-07-14".into()),
                Value::Text("+1 month".into())
            ])),
            "2023-08-14"
        );
        // Day overflow with month increment (ceiling behavior)
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-01-31".into()),
                Value::Text("+1 month".into())
            ])),
            "2023-03-03"
        );
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-07-14".into()),
                Value::Text("start of month".into())
            ])),
            "2023-07-01"
        );
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-07-14".into()),
                Value::Text("start of year".into())
            ])),
            "2023-01-01"
        );
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-07-14".into()),
                Value::Text("weekday 0".into())
            ])),
            "2023-07-16" // next Sunday after Fri Jul 14
        );
        assert_eq!(
            text(date_func(&[
                Value::Integer(1672531200),
                Value::Text("unixepoch".into())
            ])),
            "2023-01-01"
        );
    }

    #[test]
    fn strftime_formats() {
        let args = |f: &str| {
            vec![
                Value::Text(f.into()),
                Value::Text("2023-07-14 13:45:28.123".into()),
            ]
        };
        assert_eq!(text(strftime_func(&args("%Y-%m-%d"))), "2023-07-14");
        assert_eq!(text(strftime_func(&args("%H:%M:%S"))), "13:45:28");
        assert_eq!(text(strftime_func(&args("%s"))), "1689342328");
        assert_eq!(text(strftime_func(&args("%j"))), "195");
        assert_eq!(text(strftime_func(&args("%w"))), "5");
        assert_eq!(text(strftime_func(&args("%u"))), "5");
        assert_eq!(text(strftime_func(&args("%W"))), "28");
        assert_eq!(text(strftime_func(&args("%p"))), "PM");
        assert_eq!(text(strftime_func(&args("%F %T"))), "2023-07-14 13:45:28");
        assert_eq!(text(strftime_func(&args("100%%"))), "100%");
    }

    #[test]
    fn time_only_string() {
        // "12:34:56" → 2000-01-01 12:34:56 (SQLite assumes day 2000-01-01)
        assert_eq!(text(time_func(&[Value::Text("12:34:56".into())])), "12:34:56");
        assert_eq!(
            text(datetime_func(&[Value::Text("12:34:56".into())])),
            "2000-01-01 12:34:56"
        );
    }

    #[test]
    fn tz_suffix() {
        assert_eq!(
            text(datetime_func(&[Value::Text("2023-07-14 10:00:00+02:30".into())])),
            "2023-07-14 07:30:00"
        );
        assert_eq!(
            text(datetime_func(&[Value::Text("2023-07-14 10:00:00Z".into())])),
            "2023-07-14 10:00:00"
        );
    }

    #[test]
    fn null_and_garbage() {
        assert_eq!(date_func(&[Value::Null]), None);
        assert_eq!(date_func(&[Value::Text("not a date".into())]), None);
        assert_eq!(datetime_func(&[Value::Text("2023-07-14".into()), Value::Null]), None);
    }

    #[test]
    fn julian_day_numbers_as_input() {
        // JD 2460139.5 = 2023-07-14 00:00:00 (JDN 2460140)
        assert_eq!(
            text(date_func(&[Value::Real(2460139.5)])),
            "2023-07-14"
        );
        // Integer JD inputs land at noon: 2460140 = 2023-07-14 12:00
        assert_eq!(
            text(datetime_func(&[Value::Real(2460140.0)])),
            "2023-07-14 12:00:00"
        );
    }

    #[test]
    fn date_math_edge_cases() {
        // Leap year
        assert_eq!(
            text(date_func(&[
                Value::Text("2024-02-28".into()),
                Value::Text("+1 day".into())
            ])),
            "2024-02-29"
        );
        // Non-leap year
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-02-28".into()),
                Value::Text("+1 day".into())
            ])),
            "2023-03-01"
        );
        // Negative day shift
        assert_eq!(
            text(date_func(&[
                Value::Text("2023-03-01".into()),
                Value::Text("-1 day".into())
            ])),
            "2023-02-28"
        );
        // Fractional seconds
        assert_eq!(
            text(strftime_func(&[
                Value::Text("%f".into()),
                Value::Text("2023-07-14 13:45:28.123".into())
            ])),
            "28.123"
        );
    }

    #[test]
    fn end_of_month_modifier() {
        // datetime() truncates to whole seconds (SQLite behavior — the
        // .999 only shows with the 'subsec' modifier).
        assert_eq!(
            text(datetime_func(&[
                Value::Text("2023-07-14 10:20:30".into()),
                Value::Text("end of month".into())
            ])),
            "2023-07-31 23:59:59"
        );
        // With subsec, the last millisecond shows.
        assert_eq!(
            text(datetime_func(&[
                Value::Text("2023-07-14 10:20:30".into()),
                Value::Text("end of month".into()),
                Value::Text("subsec".into())
            ])),
            "2023-07-31 23:59:59.999"
        );
    }
}
