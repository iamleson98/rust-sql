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

pub mod error;
pub mod planner;
pub mod schema;
pub mod sql;
pub mod storage;
pub mod types;

pub mod executor;
pub mod api;

pub use api::{Database, Params};
pub use error::{Error, Result};
pub use types::{Affinity, Row, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
