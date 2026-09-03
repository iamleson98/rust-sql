//! Cross-statement join-build cache (pager-hosted advisory cache).
//!
//! The fused streaming hash join (`try_fused_scan_hash_join`) pays a full
//! build-side scan + decode + hash-table construction on EVERY execution.
//! For repeated read-only queries — the OLTP/reporting shape, and the
//! benchmark loop — the build side is byte-identical between executions.
//! This cache memoizes the built state (SoA values + open-addressing
//! slots + duplicate chains) keyed by (build root, wanted columns) and
//! validated against the pager's `write_epoch` — the SAME advisory-cache
//! pattern the B+tree leaf hints and the `CountStar` memoization already
//! use: any write (DML/DDL, rollback restore) bumps the epoch and every
//! cached build is re-derived on next use.

use std::collections::HashMap;
use std::sync::Arc;

use crate::types::Value;

/// One (key, head-ordinal) slot of the join's open-addressing table.
/// `key == u64::MAX` marks an empty slot.
#[derive(Clone, Copy)]
pub(crate) struct JoinSlot {
    pub key: u64,
    pub head: u32,
}

const EMPTY_SLOT: JoinSlot = JoinSlot {
    key: u64::MAX,
    head: u32::MAX,
};

/// The reusable build-side state of one fused hash join.
#[allow(dead_code)] // n_build/stride: documentation + debug assertions
pub(crate) struct JoinBuildState {
    /// `pager.write_epoch()` at build time — validity check on reuse.
    pub epoch: u64,
    /// Number of build rows stored.
    pub n_build: usize,
    /// Values per build row (SoA stride).
    pub stride: usize,
    /// Decoded build-side values, `n_build * stride` in row order.
    pub build_vals: Vec<Value>,
    /// Open-addressing table, capacity a power of two, load factor <= 0.5.
    pub slots: Vec<JoinSlot>,
    /// Duplicate chain: ordinal -> next ordinal with the same key.
    pub chain: Vec<u32>,
}

impl JoinBuildState {
    /// The empty-slot sentinel shared with the executor's local tables.
    pub fn empty_slot() -> JoinSlot {
        EMPTY_SLOT
    }
}

/// Cache map: (build root page, wanted-column list) -> built state.
/// Bounded: a fresh insert beyond [`MAX_ENTRIES`] clears the map (join
/// shapes per database are few; a clear-all is a cheap, safe policy).
pub(crate) type JoinBuildCache = HashMap<(u32, Vec<usize>), Arc<JoinBuildState>>;

/// Maximum cached join builds before a wholesale clear.
pub(crate) const MAX_JOIN_CACHE_ENTRIES: usize = 8;

/// Insert with the bounded-clear policy.
pub(crate) fn join_cache_insert(
    cache: &mut JoinBuildCache,
    key: (u32, Vec<usize>),
    state: Arc<JoinBuildState>,
) {
    if cache.len() >= MAX_JOIN_CACHE_ENTRIES && !cache.contains_key(&key) {
        cache.clear();
    }
    cache.insert(key, state);
}
