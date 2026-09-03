//! Storage layer: page format, pager, WAL, B+tree, and row codec.
//!
//! This layer owns all disk I/O. Higher layers (schema, executor) interact
//! with storage exclusively through the [`Storage`] facade, which provides
//! table scans, point lookups, inserts, deletes, and index maintenance.

pub mod btree;
pub mod integrity;
pub mod join_cache;
pub mod mvcc;
pub mod page;
pub mod pager;
pub mod row_codec;
pub mod wal;

pub use btree::{Btree, LookupResult};
pub use page::{Page, PageId, PageType, DEFAULT_PAGE_SIZE};
pub use pager::{PageRef, Pager};
pub use row_codec::{
    apply_affinities, decode_row, decode_row_into, decode_row_selective, encode_row,
    encode_row_into,
};
pub use wal::Wal;
