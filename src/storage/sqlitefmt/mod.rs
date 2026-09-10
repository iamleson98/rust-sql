//! SQLite on-disk format (fileformat2) interoperability.
//!
//! rustqlite's native `RSQLDB04` container is byte-incompatible with
//! SQLite (little-endian freelist, custom record codec, 0-based pages).
//! This module gives the engine a SECOND, complete file-format stack so
//! real SQLite `.db` files can be opened, queried, written, and handed
//! back — with the result passing `PRAGMA integrity_check` in SQLite
//! itself:
//!
//! * **reader** — parses the 100-byte header, applies committed WAL
//!   frames, walks table b-trees (rowid + WITHOUT ROWID) with exact
//!   overflow-chain reassembly, and decodes SQLite records (serial
//!   types) into engine `Value`s.
//! * **writer** — bulk-builds dense b-trees bottom-up with SQLite's
//!   separator invariants, overflow formulas and cell layouts, then
//!   commits atomically (temp + fsync + rename).
//!
//! The integration lives in `api.rs`: `Database::open` sniffs the magic
//! and transparently loads SQLite files into the engine (in-memory
//! operating state, dump-on-commit persistence), so every SQL feature
//! above the storage layer — planner, executor, parallel scans, C ABI,
//! sqlx driver — works unchanged on SQLite-created files, and files the
//! engine writes open in the `sqlite3` CLI.

pub mod header;
pub mod reader;
pub mod record;
pub mod varint;
pub mod wal;
pub mod writer;

pub use reader::{read_sqlite_file, RawRow, SchemaRow, SqliteDbImage};
pub use writer::{build_bytes, write_sqlite_file, Collation, OutDb, OutObject};

/// Cheap file-type sniff: does this path hold a SQLite-format database?
pub fn is_sqlite_file(path: &std::path::Path) -> bool {
    reader::is_sqlite_file(path)
}
