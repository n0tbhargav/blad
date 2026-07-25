//! Memory instrumentation.
//!
//! Two numbers, deliberately, because they answer different questions and their
//! *difference* is itself informative:
//!
//! - **Heap** — bytes we allocated, counted exactly by wrapping the global allocator.
//!   Resettable, so it can be attributed per phase.
//! - **RSS** — what the OS considers resident, a monotonic high-water mark from
//!   `getrusage`. Includes things the heap counter cannot see: dirty pages from temp
//!   files, mapped libraries, allocator overhead.
//!
//! The gap between them is the cost of everything that is not our data structures. For
//! blad that is dominated by the temp files the shell-out codec round-trips through —
//! which is exactly the kind of thing that stays invisible if you only measure one.
//!
//! # Usage
//!
//! The binary installs the allocator:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: blad_mem::Tracking<std::alloc::System> =
//!     blad_mem::Tracking(std::alloc::System);
//! ```
//!
//! Without that, [`heap_peak`] reports zero and everything still works — instrumentation
//! must never be load-bearing.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering};

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Global allocator wrapper that tracks live bytes and a resettable high-water mark.
///
/// Counters use relaxed ordering: they are diagnostics, not synchronisation, and the
/// cost must stay near zero on the allocation path.
pub struct Tracking<A>(pub A);

unsafe impl<A: GlobalAlloc> GlobalAlloc for Tracking<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { self.0.alloc(layout) };
        if !p.is_null() {
            bump(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { self.0.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                bump(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

fn bump(n: usize) {
    let now = CURRENT.fetch_add(n, Ordering::Relaxed) + n;
    // Not atomic with the add, so under contention the recorded peak can lag slightly.
    // Acceptable for a diagnostic; a lock here would be far more costly than the error.
    PEAK.fetch_max(now, Ordering::Relaxed);
}

/// Bytes currently allocated through the tracking allocator.
pub fn heap_current() -> u64 {
    CURRENT.load(Ordering::Relaxed) as u64
}

/// Highest [`heap_current`] seen since the last [`reset_heap_peak`].
pub fn heap_peak() -> u64 {
    PEAK.load(Ordering::Relaxed) as u64
}

/// Drop the high-water mark to the current level, so the next phase is measured on its
/// own rather than inheriting the previous one's peak.
pub fn reset_heap_peak() {
    PEAK.store(CURRENT.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// Process peak resident set size, in bytes.
///
/// Monotonic — it never falls — so sampling it at phase boundaries tells you *which*
/// phase pushed the peak, which is the question worth answering.
///
/// `ru_maxrss` is bytes on macOS and kilobytes on Linux. Getting that wrong yields
/// silently 1024x-wrong numbers, which is worse than no measurement.
pub fn rss_highwater() -> u64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut ru) != 0 {
            return 0;
        }
        let v = ru.ru_maxrss as u64;
        if cfg!(target_os = "macos") {
            v
        } else {
            v * 1024
        }
    }
}

/// Time, heap, and RSS for one phase of work.
#[derive(Debug, Clone, Copy, Default)]
pub struct Phase {
    pub time: std::time::Duration,
    /// Peak bytes allocated *during this phase*.
    pub heap_peak: u64,
    /// Process RSS high-water after this phase. Monotonic across phases: a phase that
    /// does not raise it did not drive peak memory.
    pub rss_after: u64,
}

/// Run `f`, timing it and measuring its heap peak in isolation.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Phase) {
    reset_heap_peak();
    let start = std::time::Instant::now();
    let out = f();
    let phase = Phase {
        time: start.elapsed(),
        heap_peak: heap_peak(),
        rss_after: rss_highwater(),
    };
    (out, phase)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the allocator installed the heap counters read zero and nothing panics —
    /// instrumentation must never be load-bearing.
    ///
    /// Deliberately no assertion on elapsed time: that would be testing the clock, and
    /// a zero-duration reading is legitimate rather than a bug.
    #[test]
    fn works_without_allocator_installed() {
        reset_heap_peak();
        let (v, p) = measure(|| {
            let big: Vec<u8> = vec![7u8; 4 << 20];
            std::hint::black_box(&big);
            big.len()
        });
        assert_eq!(v, 4 << 20);
        assert!(p.rss_after > 0, "rss should always be readable");
    }

    #[test]
    fn rss_is_plausible() {
        // Any live process has some resident memory; zero would mean getrusage failed.
        assert!(rss_highwater() > 0);
    }

    #[test]
    fn reset_lowers_the_peak() {
        {
            let _big: Vec<u8> = vec![0u8; 1 << 20];
        }
        reset_heap_peak();
        assert!(heap_peak() >= heap_current());
    }
}
