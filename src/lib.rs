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

/// Disable mimalloc's delayed page purging.
///
/// By default mimalloc madvises freed pages back to the OS after a 10 ms
/// idle window; the next allocation then re-faults them, costing 10-15 µs
/// on the first query after any free storm. glibc — the allocator SQLite
/// is measured against — never returns small-object pages to the OS, so it
/// never pays this tax. mimalloc's own docs recommend `-1` (never purge)
/// for latency-sensitive services; we set it once, at engine init.
#[cfg(feature = "mimalloc")]
fn tune_mimalloc() {
    use std::sync::Once;
    static TUNED: Once = Once::new();
    TUNED.call_once(|| unsafe {
        // `mi_option_purge_delay` sits at enum position 15 in mimalloc.h
        // (eager_commit_delay = 14, use_numa_nodes = 16 bracket it); the
        // sys crate's bindings don't name it, so use the raw value.
        // purge_delay (enum position 15): freed pages are madvise'd back
        // to the OS after this delay (ms). -1 = never — the old setting,
        // bought first-query latency but retained EVERY freed page in
        // VmHWM (peak-RSS-bound workloads measured ~3x their live set).
        // 25 ms = pages freed by an alloc-heavy query return to the OS
        // once the query loop goes idle, but back-to-back iterations
        // (best-of-N benchmark rounds, storm-then-read transitions well
        // under 25 ms apart) still reuse them — immediate purging (0)
        // re-faulted the entire working set on EVERY round (measured
        // +0.5 ms/round on the join+GROUP BY bench).
        // Raw enum positions from mimalloc.h (the sys crate's bindings
        // only name the pre-v3 subset; purge_delay = 15 is bracketed by
        // eager_commit_delay = 14 and use_numa_nodes = 16, allow_thp = 43
        // by page_cross_thread_max_reclaim = 42 and minimal_purge_size).
        const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
        const MI_OPTION_ALLOW_THP: libmimalloc_sys::mi_option_t = 43;
        // Never purge automatically: the per-allocation purge-timer checks
        // cost ~25 ns/alloc (measured as the UPDATE-by-PK regression), and
        // freed pages return explicitly at write-burst boundaries instead
        // (see `drain_mimalloc_wake`'s mi_collect).
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, -1);
        debug_assert_eq!(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY), -1);
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

pub mod error;
/// SQLite-style C ABI (`rustqlite_open` / `rustqlite_prepare` /
/// `rustqlite_step` / ...) plus the extension loading entry points.
pub mod ffi;
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
