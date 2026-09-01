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
        const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, -1);
        debug_assert_eq!(libmimalloc_sys::mi_option_get(MI_OPTION_PURGE_DELAY), -1);
    });
}

#[cfg(not(feature = "mimalloc"))]
fn tune_mimalloc() {}

/// Engine one-time init: allocator tuning. Called from every `Database`
/// constructor before any page is touched.
fn engine_init() {
    tune_mimalloc();
}

pub mod error;
/// OOM fault-injection allocator (`oom-injection` feature).
#[cfg(feature = "oom-injection")]
pub mod oom_alloc;
/// Plugin system: user functions, aggregates, collations, virtual-table
/// modules, page codecs (static Rust + dynamic C/C++/Zig/Rust extensions).
pub mod plugin;
/// SQLite-style C ABI (`rustqlite_open` / `rustqlite_prepare` /
/// `rustqlite_step` / ...) plus the extension loading entry points.
pub mod ffi;
/// SQLite-style streaming statement handles (`prepare` / `bind` / `step`).
pub mod statement;
pub mod planner;
pub mod schema;
pub mod sql;
pub mod storage;
pub mod types;

pub mod executor;
pub mod api;

pub use api::{Database, Params};
pub use error::{Error, Result};
pub use statement::{Statement, StepResult};
pub use types::{Affinity, Row, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
