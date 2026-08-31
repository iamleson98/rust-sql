//! OOM fault-injection allocator — the rustqlite equivalent of SQLite's
//! `SQLITE_MEMDEBUG` / memsys2 build (https://www.sqlite.org/testing.html §3.1
//! and §8.3).
//!
//! When the crate is built with the `oom-injection` feature, this module
//! replaces the global allocator with a counting allocator that can be
//! rigged to fail at (and after) a chosen allocation number:
//!
//! ```ignore
//! // in a test binary:
//! rustqlite::oom_alloc::set_fail_at(1234);   // allocation #1234+ return null
//! let n = rustqlite::oom_alloc::allocation_count();
//! ```
//!
//! Allocation failure makes the process abort (`handle_alloc_error`) — the
//! caller then verifies, externally, that the database file survived. That
//! is exactly SQLite's OOM loop: fail at N, verify, N += 1, repeat.
//!
//! Enabling the feature also disables mimalloc for that build (only one
//! `#[global_allocator]` may exist per binary), so `cargo test
//! --features oom-injection` runs the whole suite on the System allocator
//! with injection available — a second useful configuration to test
//! (SQLite runs its suite under several allocator configurations too).
//!
//! The allocator is wait-free (two relaxed atomics per allocation) and,
//! with `set_fail_at(usize::MAX)` (the default), never fails.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fail every allocation at index >= this value. `usize::MAX` = never fail.
static FAIL_AT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Number of allocations served so far (monotonic for the process).
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Rig the allocator to fail starting at allocation `n` (0-based).
/// Every allocation from index `n` onward returns null. Passing
/// `usize::MAX` (the default) disables injection.
pub fn set_fail_at(n: usize) {
    FAIL_AT.store(n, Ordering::Relaxed);
}

/// Current fail point (`usize::MAX` = injection disabled).
pub fn fail_at() -> usize {
    FAIL_AT.load(Ordering::Relaxed)
}

/// Number of allocations performed so far by this process.
pub fn allocation_count() -> usize {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

/// Reset the allocation counter (used by calibration harnesses that
/// measure the allocation footprint of a specific workload).
pub fn reset_allocation_count() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
}

/// The injecting global allocator installed by the `oom-injection` feature.
pub struct OomAllocator;

unsafe impl GlobalAlloc for OomAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let n = ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        if n >= FAIL_AT.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        System.alloc(layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let n = ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        if n >= FAIL_AT.load(Ordering::Relaxed) {
            return core::ptr::null_mut();
        }
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Only growth consumes an allocation from the fault budget.
        if new_size > layout.size() {
            let n = ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            if n >= FAIL_AT.load(Ordering::Relaxed) {
                return core::ptr::null_mut();
            }
        }
        System.realloc(ptr, layout, new_size)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}
