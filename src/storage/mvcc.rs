//! Multi-Version Concurrency Control (MVCC).
//!
//! MVCC gives us snapshot isolation: each transaction sees a consistent
//! snapshot of the database taken at the transaction's start time. Readers
//! never block writers, and writers never block readers.
//!
//! Implementation strategy:
//! - Each transaction has a monotonic Transaction ID (TXID).
//! - The WAL is the source of truth for committed writes; the main DB file
//!   is a checkpoint of the WAL.
//! - A "snapshot" is the set of page versions visible to a transaction.
//!   We track this via the WAL frame index: a snapshot at TXID `t` sees
//!   the latest version of each page from frames committed at TXID <= `t`.
//! - On checkpoint, we apply the WAL to the main file and reset.
//!
//! This is simpler than PostgreSQL's MVCC (which keeps tuple-level version
//! chains) but gives us snapshot isolation at the page level, which is
//! sufficient for an embedded database.

use crate::error::Result;
use crate::storage::page::PageId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic transaction ID.
pub type Txid = u64;

/// Global counter for transaction IDs.
static NEXT_TXID: AtomicU64 = AtomicU64::new(1);

/// Allocate the next transaction ID.
pub fn next_txid() -> Txid {
    NEXT_TXID.fetch_add(1, Ordering::SeqCst)
}

/// A snapshot of the database state at a given TXID.
///
/// In our implementation, a snapshot is just the TXID + the number of
/// committed WAL frames at the time the snapshot was taken. Reads consult
/// the WAL up to that frame index.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub txid: Txid,
    pub wal_frame_count: u32,
}

impl Snapshot {
    pub fn new(txid: Txid, wal_frame_count: u32) -> Self {
        Self { txid, wal_frame_count }
    }
}

/// Tracks the latest version of each page across all active snapshots.
///
/// When a page is modified, we keep the old version in memory until all
/// snapshots that might read it have finished. In practice, we just rely
/// on the WAL: old versions are still there until checkpoint.
pub struct VersionTracker {
    /// For each page, the WAL frame indices where it was written.
    /// Used to find the right version of a page for a given snapshot.
    page_versions: HashMap<PageId, Vec<u32>>,
}

impl VersionTracker {
    pub fn new() -> Self {
        Self { page_versions: HashMap::new() }
    }

    /// Record that a page was written at the given WAL frame index.
    pub fn record_write(&mut self, page_id: PageId, frame: u32) {
        self.page_versions.entry(page_id).or_default().push(frame);
    }

    /// Find the latest frame index <= `max_frame` where `page_id` was written.
    /// Returns None if the page has no WAL writes (use the main DB file).
    pub fn latest_version(&self, page_id: PageId, max_frame: u32) -> Option<u32> {
        let versions = self.page_versions.get(&page_id)?;
        // Binary search for the largest frame <= max_frame.
        let pos = versions.partition_point(|&f| f <= max_frame);
        if pos == 0 {
            None
        } else {
            Some(versions[pos - 1])
        }
    }

    /// Discard version entries that are no longer needed (after a checkpoint).
    pub fn truncate(&mut self, frames_to_discard: u32) {
        for versions in self.page_versions.values_mut() {
            versions.retain(|&f| f >= frames_to_discard);
            // Shift remaining frame indices down.
            for f in versions.iter_mut() {
                *f -= frames_to_discard;
            }
        }
        self.page_versions.retain(|_, v| !v.is_empty());
    }
}

impl Default for VersionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// A no-op result placeholder for now.
pub type MvccResult<T> = Result<T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tracker_finds_latest() {
        let mut t = VersionTracker::new();
        t.record_write(1, 5);
        t.record_write(1, 10);
        t.record_write(1, 20);
        assert_eq!(t.latest_version(1, 0), None);
        assert_eq!(t.latest_version(1, 5), Some(5));
        assert_eq!(t.latest_version(1, 9), Some(5));
        assert_eq!(t.latest_version(1, 15), Some(10));
        assert_eq!(t.latest_version(1, 100), Some(20));
    }
}
