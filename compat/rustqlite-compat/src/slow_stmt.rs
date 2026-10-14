//! Slow-statement observation for the C-ABI surface.
//!
//! sqlx / sea-orm log statement TEXT at debug level but never the wall
//! time a statement took on the engine — production incidents ("search
//! takes 28 s", "the service eats gigabytes of RAM") then have to be
//! root-caused by guesswork. This hook times every statement executed
//! through `sqlite3_step` (SELECTs at their first step — the engine
//! materializes the result set there — and DML through `run_once`) and
//! emits one `tracing::warn!` per statement over the threshold, carrying
//! the kind, the elapsed time, the returned/affected row count and a
//! truncated statement body.
//!
//! The threshold comes from `RUSTQLITE_SLOW_STMT_MS` (default 1000 =
//! the "any statement > 1 s must be identifiable from the logs" bar;
//! `0` disables the hook). Reads once, cached for the process lifetime —
//! the knob is an ops lever, not SQL semantics.
//!
//! Events go through `tracing`, so they follow the host's subscriber:
//! the datxevui backend runs `RUST_LOG=info` and captures the warnings
//! in its normal log stream; the engine's own tests and the CLI, which
//! install no subscriber, pay only the timing call.

use std::sync::OnceLock;
use std::time::Instant;

/// How much of the statement body survives into the log line.
const SQL_HEAD: usize = 240;

/// Byte-truncate at a char boundary (statement text is usually ASCII,
/// but a multi-byte constant can sit right at the cut).
fn head_of(sql: &str) -> String {
    if sql.len() <= SQL_HEAD {
        return sql.to_string();
    }
    let mut cut = SQL_HEAD;
    while cut > 0 && !sql.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &sql[..cut])
}

fn threshold_ms() -> u64 {
    static T: OnceLock<u64> = OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("RUSTQLITE_SLOW_STMT_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1000)
    })
}

/// Call right after a statement's engine work finished. `kind` is a
/// short label ("select" / "execute"); `rows` is the produced row count
/// when known (SELECT: result rows; DML: 0 = changes are read
/// separately by the caller and stay out of the message).
pub fn observe(start: Instant, sql: &str, kind: &str, rows: usize) {
    let ms = threshold_ms();
    if ms == 0 {
        return;
    }
    let elapsed = start.elapsed();
    if elapsed.as_millis() >= u128::from(ms) {
        let head = head_of(sql);
        tracing::warn!(
            target: "rustqlite::slow_stmt",
            kind,
            rows,
            ms = elapsed.as_millis() as u64,
            "slow statement: {head}"
        );
    }
}
