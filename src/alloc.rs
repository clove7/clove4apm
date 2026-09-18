//! Counting global allocator (feature `alloc-obs`).
//!
//! The consumer installs it once:
//! ```ignore
//! #[global_allocator]
//! static A: clove4apm::alloc::CountingAlloc = clove4apm::alloc::CountingAlloc;
//! ```
//!
//! # What the numbers answer
//!
//! `live_bytes() = allocated - freed` compared against RSS separates live data
//! from allocator retention: a large gap means the allocator holds pages it no
//! longer has data for — fragmentation, not a leak in your own structures. The
//! two have opposite fixes, and no process-level metric can tell them apart.
//!
//! The counts answer a different question: churn. Millions of allocations per
//! second with flat `live_bytes` is pure overhead — memory being recycled
//! rather than needed — and it never shows up in a memory graph at all.
//!
//! # Cost
//!
//! Two relaxed atomic adds per allocation (bytes and count). Relaxed ordering
//! is deliberate: these are statistics, and a counter that is a few allocations
//! stale is fine, while the fences needed to make it exact would cost more than
//! the information is worth.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static DEALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

pub struct CountingAlloc;

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Only the difference moves: a realloc is not a new allocation, and
        // counting it as one would make `live_bytes` drift upward forever on
        // any growing Vec.
        if new_size >= layout.size() {
            ALLOCATED.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        } else {
            FREED.fetch_add((layout.size() - new_size) as u64, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Bytes currently held by the allocator on behalf of live values.
pub fn live_bytes() -> u64 {
    ALLOCATED
        .load(Ordering::Relaxed)
        .saturating_sub(FREED.load(Ordering::Relaxed))
}

/// `(allocated_total, freed_total)` bytes since process start.
pub fn totals() -> (u64, u64) {
    (
        ALLOCATED.load(Ordering::Relaxed),
        FREED.load(Ordering::Relaxed),
    )
}

/// `(alloc_count, dealloc_count)` since process start.
pub fn counts() -> (u64, u64) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        DEALLOC_COUNT.load(Ordering::Relaxed),
    )
}

// The counters only see allocations if this allocator is the global one —
// install it for the test binary, otherwise the tests below prove nothing.
#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: CountingAlloc = CountingAlloc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_allocations_and_frees() {
        let (a0, f0) = totals();
        let (ac0, dc0) = counts();
        let v: Vec<u8> = vec![0; 4096];
        let (a1, _) = totals();
        let (ac1, _) = counts();
        assert!(a1 > a0, "allocation bytes not counted");
        assert!(ac1 > ac0, "allocation count not counted");
        drop(v);
        let (_, f1) = totals();
        let (_, dc1) = counts();
        assert!(f1 > f0, "free bytes not counted");
        assert!(dc1 > dc0, "free count not counted");
    }

    #[test]
    fn realloc_counts_only_the_difference() {
        let _guard = crate::memory_test_lock();
        // The counters are process-global and the test binary is multi-threaded,
        // so this drives the allocator directly at a size where concurrent test
        // noise (kilobytes) cannot reach the assertion (tens of megabytes).
        //
        // What it protects: counting a realloc as a whole new allocation would
        // make `live_bytes` climb without bound on any growing Vec — the single
        // most common shape in real code — and the leak detector would then
        // report a leak in every healthy program.
        const SMALL: usize = 32 << 20;
        const BIG: usize = 96 << 20;
        const NOISE: u64 = 8 << 20;

        let layout = Layout::from_size_align(SMALL, 8).unwrap();
        let before = live_bytes();

        unsafe {
            let p = CountingAlloc.alloc(layout);
            assert!(!p.is_null());
            let after_alloc = live_bytes().saturating_sub(before);
            assert!(
                after_alloc.abs_diff(SMALL as u64) < NOISE,
                "alloc counted {after_alloc}, expected about {SMALL}"
            );

            let p = CountingAlloc.realloc(p, layout, BIG);
            assert!(!p.is_null());
            let after_grow = live_bytes().saturating_sub(before);
            assert!(
                after_grow.abs_diff(BIG as u64) < NOISE,
                "after growing to {BIG} the live total moved by {after_grow} \
                 — a realloc counted as a fresh allocation would show about {}",
                SMALL + BIG
            );

            let grown = Layout::from_size_align(BIG, 8).unwrap();
            CountingAlloc.dealloc(p, grown);
        }

        // Freeing the grown block must return live to where it started, or the
        // realloc accounting was asymmetric.
        let leaked = live_bytes().saturating_sub(before);
        assert!(leaked < NOISE, "live stayed {leaked} above the baseline");
    }
}
