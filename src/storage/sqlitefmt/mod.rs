//! SQLite on-disk format (fileformat2) — the INTERCHANGE surface.
//!
//! rustqlite's storage is the native `RSQLDB0x` container; SQLite's own
//! file format is an interchange format this module reads and writes
//! ONE-SHOT (there is deliberately no live in-place SQLite-format
//! container — opening a SQLite-format file loads it for interop, and
//! the first write commit ADOPTS the path into the native container;
//! see `Database::from_sqlite_file` / `Pager::adopt_file_backing`):
//!
//! * **reader** — parses the 100-byte header, applies committed WAL
//!   frames, walks table b-trees (rowid + WITHOUT ROWID) with exact
//!   overflow-chain reassembly, and decodes SQLite records (serial
//!   types) into engine `Value`s. Used by `Database::open` (magic
//!   sniff) for every SQLite-created file.
//! * **writer** — bulk-builds dense b-trees bottom-up with SQLite's
//!   separator invariants, overflow formulas and cell layouts, then
//!   commits atomically (temp + fsync + rename). Powers
//!   `Database::export_sqlite_format`, `VACUUM INTO` and the
//!   `.archive` CLI's file format.
//! * **rj** — hot ROLLBACK-journal replay at open: a crashed REAL
//!   SQLite writer on this file left pre-images that restore the
//!   pre-transaction state (SQLite's own open-time `pager_playback`).
//!
//! Keeping reader/writer one-shot is the point: whole-image costs are
//! paid once at a boundary (open / export), never per commit.

pub mod header;
pub mod reader;
pub mod record;
pub mod rj;
pub mod varint;
pub mod writer;

pub use reader::{read_sqlite_file, RawRow, SchemaRow, SqliteDbImage};
pub use writer::{build_bytes, write_sqlite_file, Collation, OutDb, OutObject};

/// Cheap file-type sniff: does this path hold a SQLite-format database?
pub fn is_sqlite_file(path: &std::path::Path) -> bool {
    reader::is_sqlite_file(path)
}
