//! # rustqlite
//!
//! An embedded SQL database engine written from scratch in pure Rust, modeled
//! after SQLite but designed for cleaner code, better performance, and lower
//! memory usage.
//!
//! ## Architecture
//!
//! The engine is structured in five layers, each with a clear contract:
//!
//! 1. **Storage** (`storage`): page format, pager (file I/O + LRU cache),
//!    B+tree, WAL, MVCC, and row codec.
//! 2. **Schema** (`schema`): catalog of tables, indexes, views, triggers.
//! 3. **SQL** (`sql`): lexer, parser, and AST.
//! 4. **Planner** (`planner`): AST → logical plan with name resolution and
//!    simple optimizations (predicate pushdown, index selection).
//! 5. **Executor** (`executor`): Volcano-style iterator model that pulls rows
//!    through the plan tree.
//!
//! The public API is in `api` (`Database`, `Connection`).
//!
//! ## Quick Start
//!
//! ```no_run
//! use rustqlite::{Database, Value};
//!
//! let mut db = Database::open("/tmp/my.db").unwrap();
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", []).unwrap();
//! db.execute("INSERT INTO users (name) VALUES ('Alice')", []).unwrap();
//! let rows = db.query("SELECT * FROM users", []).unwrap();
//! ```

#![allow(clippy::needless_lifetimes)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::large_enum_variant)]

/// Global allocator: mimalloc. The engine allocates heavily on hot paths
/// (one Vec per decoded row, statement ASTs, join key buffers, combined
/// rows). mimalloc's thread-local free lists make those small allocations
/// 20-40% cheaper than the system malloc, which translates directly into
/// scan/insert/join throughput. Opt out at build time with
/// `default-features = false`.
///
/// With the `oom-injection` feature (SQLite's SQLITE_MEMDEBUG/memsys2
/// equivalent), mimalloc is replaced by [`crate::oom_alloc::OomAllocator`]
/// so test harnesses can rig allocation failures — see `oom_alloc.rs`.
#[cfg(all(feature = "mimalloc", not(feature = "oom-injection")))]
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Fault-injecting allocator for OOM testing (`oom-injection` feature).
#[cfg(feature = "oom-injection")]
#[global_allocator]
static GLOBAL_OOM_ALLOC: crate::oom_alloc::OomAllocator = crate::oom_alloc::OomAllocator;

/// mimalloc option ids used by [`tune_mimalloc`], derived from the
/// COMPILE-TIME layout the sys crate builds, so a feature flip
/// (`libmimalloc-sys/v2`) or an upstream version bump cannot silently
/// mis-target an option.
///
/// The sys crate deliberately does not export `mi_option_purge_delay` or
/// `mi_option_allow_thp` ("experimental options... are not exposed as
/// constants for stability reasons"), so their positions come from
/// mimalloc.h — and they DIVERGE between the two trees the sys crate can
/// build:
///
/// ```text
/// option           v2 (`v2` feature)   v3 (default build)
/// ---------------  -----------------   -----------------
/// purge_delay      15                  15
/// allow_thp        37                  43
/// _mi_option_last  38                  47
/// ```
///
/// `mi_option_set` silently IGNORES out-of-range ids (hard bounds guard
/// in mimalloc's options.c: `if (option < 0 || option >=
/// _mi_option_last) return;`), so hardcoding the v3 `allow_thp` (43) in
/// a v2 build turned the THP disable into a SILENT NO-OP — mimalloc kept
/// making huge-page decisions with only the kernel-side prctl still
/// applying. `purge_delay` happens to sit at 15 in BOTH layouts
/// (bracketed by `eager_commit_delay`/`deprecated_eager_commit_delay`
/// = 14 and `use_numa_nodes` = 16), which is why the purge-delay knob
/// worked either way. Deriving both positions from `_mi_option_last`
/// (which the sys crate DOES export, one value per layout) makes the
/// pairing explicit; an unknown future layout is a COMPILE error (const
/// `panic!`) instead of a silently-wrong or silently-dropped write.
#[cfg(feature = "mimalloc")]
const fn mimalloc_option_positions() -> (libmimalloc_sys::mi_option_t, libmimalloc_sys::mi_option_t)
{
    match libmimalloc_sys::_mi_option_last {
        38 => (15, 37), // mimalloc v2 (libmimalloc-sys built with `v2`)
        47 => (15, 43), // mimalloc v3 (default build)
        // Layout drift: upstream inserted or removed an option before
        // these positions. Refuse to compile rather than write to a
        // possibly-wrong slot — re-derive against the bundled
        // c_src/mimalloc/v{N}/include/mimalloc.h of the resolved
        // libmimalloc-sys, and update the table above.
        _ => panic!("mimalloc option layout changed: re-derive the option positions in mimalloc_option_positions()"),
    }
}

/// `mi_option_purge_delay` position for the layout being built.
#[cfg(feature = "mimalloc")]
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = mimalloc_option_positions().0;

/// `mi_option_allow_thp` position for the layout being built.
#[cfg(feature = "mimalloc")]
const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = mimalloc_option_positions().1;

// In-bounds guarantees for the derived positions (mimalloc's set/get
// silently drop out-of-range ids, so a slipped constant must fail HERE,
// at compile time, not vanish at runtime).
#[cfg(feature = "mimalloc")]
const _: () = assert!(
    MI_OPTION_PURGE_DELAY < libmimalloc_sys::_mi_option_last,
    "purge_delay position out of bounds for this mimalloc layout"
);
#[cfg(feature = "mimalloc")]
const _: () = assert!(
    MI_OPTION_ALLOW_THP < libmimalloc_sys::_mi_option_last,
    "allow_thp position out of bounds for this mimalloc layout"
);

/// Disable mimalloc's delayed page purging.
///
/// By default mimalloc madvises freed pages back to the OS after a 10 ms
/// idle window; the next allocation then re-faults them, costing 10-15 µs
/// on the first query after any free storm. glibc — the allocator SQLite
/// is measured against — never returns small-object pages to the OS, so it
/// never pays this tax. mimalloc's own docs recommend `-1` (never purge)
/// for latency-sensitive services; we set it once, at engine init.
///
/// Deployment override: `RUSTSQL_MIMALLOC_PURGE_DELAY_MS=<ms>` opts a
/// service into delayed purging (25 is the measured sweet spot — see
/// the option comment). Unset / unparsable → -1, the latency-first
/// default, so nothing changes unless an operator asks for it.
///
/// NOTE: setting this programmatically STOMPS mimalloc's own
/// `MIMALLOC_PURGE_DELAY` env var (if anything read it first) — the
/// engine-owned `RUSTSQL_MIMALLOC_PURGE_DELAY_MS` is the supported knob.
#[cfg(feature = "mimalloc")]
#[allow(clippy::useless_conversion)] // c_long == i64 on unix: the
                                     // i64::from() widenings below are
                                     // no-ops there but REQUIRED on
                                     // Windows (c_long == i32)
fn tune_mimalloc() {
    use std::sync::Once;
    static TUNED: Once = Once::new();
    TUNED.call_once(|| unsafe {
        // purge_delay: freed pages are madvise'd back to the OS after
        // this delay (ms). -1 = never — the old setting, bought
        // first-query latency but retained EVERY freed page in VmHWM
        // (peak-RSS-bound workloads measured ~3x their live set).
        // 25 ms = pages freed by an alloc-heavy query return to the OS
        // once the query loop goes idle, but back-to-back iterations
        // (best-of-N benchmark rounds, storm-then-read transitions well
        // under 25 ms apart) still reuse them — immediate purging (0)
        // re-faulted the entire working set on EVERY round (measured
        // +0.5 ms/round on the join+GROUP BY bench).
        //
        // The option POSITION is layout-derived (see
        // [`mimalloc_option_positions`]): 15 in both v2 and v3, but the
        // sys crate does not export the name, and hardcoding it blind
        // is how allow_thp drifted (below).
        //
        // Deployment-tunable, default -1 (never purge automatically):
        // the per-allocation purge-timer checks cost ~25 ns/alloc
        // (measured as the UPDATE-by-PK regression), and freed pages
        // return explicitly at write-burst boundaries instead (see
        // `drain_mimalloc_wake`'s mi_collect).
        //
        // WHY an override exists at all: the write-burst drains + the
        // SERVICE-side periodic mi_collect cannot return pages stranded
        // in OTHER live threads' mimalloc heaps — and every thread-local
        // heap that ever served a big transient burst (argon2 verifies,
        // tantivy searches, request JSON) holds those freed pages forever
        // when purging is off (measured in production 2026-09-21: a
        // login burst ratcheted +2.5 MB PER LOGIN, permanent — the
        // "RSS only ever grows" report). Services that value RSS
        // hygiene over last-few-percent latency set
        // RUSTSQL_MIMALLOC_PURGE_DELAY_MS=25 and freed pages madvise back
        // after ~25 ms of thread idleness.
        let purge_delay_ms: i64 = std::env::var("RUSTSQL_MIMALLOC_PURGE_DELAY_MS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(-1);
        // PORTABILITY: the FFI takes c_long — i64 on LP64 unix, i32 on
        // Windows (LLP64). The plain i64 argument compiled on unix and
        // broke every Windows job; route through std::ffi::c_long so the
        // conversion is explicit on every target. The value class here
        // (-1 or a small ms count) fits both widths trivially.
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, purge_delay_ms as std::ffi::c_long);
        debug_assert_eq!(
            i64::from(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY)),
            purge_delay_ms
        );
        // NOTE on the startup footprint (measured, not changed here):
        // mimalloc commits memory at 64 KiB page granularity per size
        // class — the engine's ~0.4 MiB of open-path allocations spread
        // across ~15 size classes, so the first `Database::open` shows
        // ~1 MiB of RSS under mimalloc vs ~0.4 MiB under glibc. That
        // granularity is the documented trade for mimalloc's 20-40%
        // small-allocation speed advantage (SQLite is measured against
        // glibc). `default-features = false` opts out. (eager_commit,
        // the obvious-sounding option, is deprecated and a no-op in
        // mimalloc 2.1.)
        // Transparent Huge Pages: the kernel backs the arena's first touch
        // with 2 MiB huge pages, so a handful of touched bytes materialize
        // megabytes of RSS (measured +3-4 MB at process start, plus the
        // same amplification on every big allocation phase). Disabling THP
        // puts RSS back at exactly-touched-pages granularity. Applies to
        // segments committed after this point; for the initial arena too,
        // set MIMALLOC_ARENA_RESERVE=64M (the lean-startup recipe).
        // The position is layout-derived: 43 in v3 but 37 in v2 — the
        // previously hardcoded 43 was a SILENT NO-OP under a v2 feature
        // build (mimalloc bounds-guards the set), leaving mimalloc's own
        // THP decisions enabled while only the kernel prctl applied.
        libmimalloc_sys::mi_option_set(MI_OPTION_ALLOW_THP, 0);
        debug_assert_eq!(libmimalloc_sys::mi_option_get(MI_OPTION_ALLOW_THP), 0);
    });
}

#[cfg(not(feature = "mimalloc"))]
fn tune_mimalloc() {}

/// Disable transparent huge pages for this process (Linux).
///
/// On `THP=always` kernels every page fault inside an aligned region —
/// mimalloc's 64 MiB arena reservations included — materializes a 2 MiB
/// huge page: a touched 4 KiB allocation page costs 2 MiB of RSS, so
/// small workloads measure 2-4x their live set. mimalloc's own
/// `allow_thp=0` path prctl's exactly this, but only at mimalloc INIT —
/// which happens on the first Rust-runtime allocation, long before any
/// engine code can set the option. Doing the prctl here covers every
/// page fault after the first `Database` constructor (the runtime's
/// pre-open footprint is a few hundred KiB and already materialized).
///
/// Per-process (never system-wide), matching what mimalloc itself would
/// do with `MIMALLOC_ALLOW_THP=0` in the environment at exec time.
#[cfg(target_os = "linux")]
fn disable_thp_for_process() {
    use std::sync::Once;
    static DONE: Once = Once::new();
    DONE.call_once(thp_prctl);
}

#[cfg(not(target_os = "linux"))]
fn disable_thp_for_process() {}

/// The raw prctl (idempotent).
///
/// Early execution matters: mimalloc initializes on the first allocation
/// — before `main` — so the option-based path inside mimalloc can never
/// see our `allow_thp=0`. An `.init_array` constructor runs before the
/// Rust runtime's own allocations, covering the entire process.
/// `RSQL_ALLOW_THP=1` in the environment opts out (huge-page-friendly
/// throughput builds, `madvise`-configured kernels where THP only
/// applies to explicitly madvised regions, or profiling setups).
#[cfg(target_os = "linux")]
fn thp_prctl() {
    unsafe {
        extern "C" {
            fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
        }
        // PR_SET_THP_DISABLE = 41 (linux/prctl.h). Fails harmlessly on
        // kernels without THP support; the return value is advisory.
        prctl(41, 1, 0, 0, 0);
    }
}

/// C-constructor: run the THP prctl before the Rust runtime allocates.
#[cfg(target_os = "linux")]
#[used]
#[link_section = ".init_array"]
static THP_DISABLE_CTOR: extern "C" fn() = thp_ctor;

#[cfg(target_os = "linux")]
extern "C" fn thp_ctor() {
    // `RSQL_ALLOW_THP=1` (any non-empty value) opts out: huge-page
    // throughput builds, `madvise`-configured kernels where THP only
    // applies to explicitly madvised regions, or profiling setups.
    // getenv in a ctor is safe and allocation-free; a ctor allocation
    // would itself be THP-backed — the exact thing being avoided.
    unsafe {
        extern "C" {
            fn getenv(name: *const u8) -> *const u8;
        }
        let v = getenv(b"RSQL_ALLOW_THP\0".as_ptr());
        if !v.is_null() {
            let mut len = 0usize;
            while *v.add(len) != 0 {
                len += 1;
            }
            if len > 0 {
                return;
            }
        }
    }
    thp_prctl();
}

/// Engine one-time init: allocator + process memory-policy tuning.
/// Called from every `Database` constructor before any page is touched.
fn engine_init() {
    tune_mimalloc();
    disable_thp_for_process();
}

/// SCRAM-SHA-256 authentication (Postgres's exact SASL mechanism,
/// RFC 7677) for `rustqlite-server`, plus the client side used by
/// `rustqlite-cli --connect`: verifiers, the handshake state machines,
/// the user-store file, and bearer-session tokens.
#[cfg(feature = "auth")]
pub mod auth;
pub mod error;
/// SQLite-style C ABI (`rustqlite_open` / `rustqlite_prepare` /
/// `rustqlite_step` / ...) plus the extension loading entry points.
pub mod ffi;
/// Minimal dependency-free JSON for the HTTP server's and CLI's wire
/// protocol (request parsing, response formatting, remote-mode result
/// decoding). Not part of the SQL engine proper.
pub mod json;
/// OOM fault-injection allocator (`oom-injection` feature).
#[cfg(feature = "oom-injection")]
pub mod oom_alloc;
pub mod planner;
/// Plugin system: user functions, aggregates, collations, virtual-table
/// modules, page codecs (static Rust + dynamic C/C++/Zig/Rust extensions).
pub mod plugin;
/// Row-level pre-change events (SQLite's preupdate-hook family): see
/// `Database::set_preupdate_hook` and [`preupdate::PreupdateEvent`].
pub mod preupdate;
pub mod schema;
pub mod sql;
/// Native sqlx driver: sqlx-core's `Database` traits implemented directly
/// against the engine, so `rustqlite` works as a sqlx backend with **no C
/// ABI, no `libsqlite3.so`, no `[patch.crates-io]`** — just
/// `rustqlite = { features = ["sqlx"] }`. See `rustqlite::sqlx_driver`.
#[cfg(feature = "sqlx")]
pub mod sqlx_driver;
/// SQLite-style streaming statement handles (`prepare` / `bind` / `step`).
pub mod statement;
pub mod storage;
pub mod types;

pub mod api;
pub mod executor;

pub use api::{Database, Params};
pub use error::{Error, Result};
pub use statement::{Statement, StepResult};
/// SQLite disk-format interop surface (see `storage::sqlitefmt`).
pub use storage::sqlitefmt;
pub use types::{Affinity, Row, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(all(test, feature = "mimalloc"))]
mod mimalloc_tuning_tests {
    use super::*;

    /// `tune_mimalloc` must actually reach the slots it targets: after
    /// the Once-guarded call, both options read back the engine's policy
    /// values (the env override honored for purge delay). A feature flip
    /// or a sys-crate bump that broke the derived positions fails HERE
    /// at CI time — in release builds the same check exists only as the
    /// (stripped) `debug_assert`s, and a dropped option set is otherwise
    /// a SILENT no-op (mimalloc bounds-guards option ids).
    #[test]
    #[allow(clippy::useless_conversion)] // i64::from(c_long): no-op on
                                         // unix, required on Windows
    fn tuned_options_read_back() {
        tune_mimalloc(); // Once-guarded: idempotent, safe from tests
        let expected_purge: i64 = std::env::var("RUSTSQL_MIMALLOC_PURGE_DELAY_MS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(-1);
        unsafe {
            assert_eq!(
                i64::from(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY)),
                expected_purge,
                "purge_delay did not read back the tuned value — position mis-derived?"
            );
            assert_eq!(
                i64::from(libmimalloc_sys::mi_option_get(MI_OPTION_ALLOW_THP)),
                0,
                "allow_thp did not read back 0 — position mis-derived for this layout?"
            );
        }
    }

    /// The compile-time dispatch only knows two layouts. This pins the
    /// pairing it relies on: v2 builds export `_mi_option_last == 38`
    /// and v3 builds `== 47`. If a future sys crate lands a new layout,
    /// [`mimalloc_option_positions`] refuses to compile — this test is
    /// the belt to that braces for anyone who "fixes" the panic by
    /// guessing a position.
    #[test]
    fn layout_is_one_of_the_known_two() {
        // Bind through a local so this stays a RUNTIME check (the test
        // belt): a bare const comparison const-folds, and clippy then
        // demands it be hoisted to `const _` — which would collapse the
        // belt into the same compile-time mechanism as the braces
        // (`mimalloc_option_positions`'s const panic).
        let last = libmimalloc_sys::_mi_option_last;
        assert!(
            last == 38 || last == 47,
            "unknown mimalloc option layout (last={last}): re-derive positions",
        );
    }
}
