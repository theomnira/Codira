//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Root regions: memory outside every stack that the collector scans
//!   conservatively, like a parked thread's stack. The task runtime keeps the
//!   results of finished tasks in one until the task that joins them has copied
//!   them out (`codira_runtime::tasks::group`).
//!
//! # Why
//!
//! A value a finished task returned may refer to objects on the GC heap (a
//! `String`, an array, an `AnyError`). Between the moment the task's frames
//! are gone and the moment the joining task has copied the value onto its
//! own stack, the only copy lives in runtime memory the collector would not
//! otherwise see. A collection in that window would free the objects.
//!
//! # Protocol
//!
//! A region is a growable vector of words behind a lock. The collector reads
//! every registered region after the world is stopped (see
//! `safepoint::stop_the_world`), so it needs the region's lock. That is only
//! deadlock-free if nobody ever *holds* a region's lock while parked: every
//! access ([`RootRegion::with`]) is a short critical section that neither
//! allocates on the GC heap, nor blocks, nor reaches a safepoint. Writers
//! that store references must be running mutators (hold a
//! `safepoint::MutatorGuard` outside a blocking region): a collection then
//! cannot be marking while they write.
//!
//! A region is unregistered when its last [`Arc`] is dropped.
//!
//! # Spans: memory scanned in place
//!
//! A [`RootSpan`] registers memory that lives elsewhere -- an address and a
//! length -- so that it is scanned where it is instead of being copied into
//! a region's vector. The region arenas of `@region` functions
//! (`gc::region`) register each chunk as a span while the region is open, so
//! the GC references their objects hold keep the heap objects alive. The
//! rules are the same: registering and unregistering are short critical
//! sections that never park, and only a running mutator stores references
//! into a span. A span is unregistered when it is dropped, which must happen
//! before its memory is freed.
//!
//! Cost: every word of every span is read on every collection (and sorted
//! with the other conservative words, as stack words are). A precise
//! collector will either keep scanning spans conservatively -- sound, since
//! objects do not move -- or need maps of the reference slots of the
//! objects in them, which an arena could provide from the types it
//! allocates; a per-chunk dirty bit would let a collection skip chunks that
//! hold no references. Neither exists yet.
use std::sync::{Arc, Weak};

use parking_lot::{const_mutex, Mutex};

/// Every live region. Dead entries are pruned whenever a region is
/// registered or the collector reads the list.
static REGIONS: Mutex<Vec<Weak<RootRegion>>> = const_mutex(Vec::new());

/// Words the collector treats as potential references.
pub struct RootRegion {
    words: Mutex<Vec<usize>>,
}

impl RootRegion {
    /// A new, empty region, registered with the collector.
    pub fn new() -> Arc<RootRegion> {
        let region = Arc::new(RootRegion {
            words: Mutex::new(Vec::new()),
        });
        let mut regions = REGIONS.lock();
        regions.retain(|weak| weak.strong_count() > 0);
        regions.push(Arc::downgrade(&region));
        region
    }

    /// Runs `f` on the region's words. `f` must not allocate on the GC
    /// heap, block or reach a safepoint (see the module documentation); if
    /// it stores references, the caller must be a running mutator.
    pub fn with<R>(&self, f: impl FnOnce(&mut Vec<usize>) -> R) -> R {
        f(&mut self.words.lock())
    }
}

/// Appends the words of every registered region, and of every registered
/// span, to `out`. Called by the collector with every mutator parked.
pub(crate) fn collect_words(out: &mut Vec<usize>) {
    {
        let mut regions = REGIONS.lock();
        regions.retain(|weak| weak.strong_count() > 0);
        for region in regions.iter().filter_map(Weak::upgrade) {
            out.extend_from_slice(&region.words.lock());
        }
    }
    span_words(out);
}

/// Every registered span, start address to end address (exclusive).
static SPANS: Mutex<Vec<(usize, usize)>> = const_mutex(Vec::new());

/// Memory outside the heap and outside every stack that the collector
/// scans in place, conservatively, while this value lives (see the module
/// documentation).
#[derive(Debug)]
pub struct RootSpan {
    start: usize,
    end: usize,
}

impl RootSpan {
    /// Registers `[start, start + len)`.
    ///
    /// # Safety
    ///
    /// The memory must stay mapped and readable until the span is dropped,
    /// and `start` must be aligned to a word.
    pub unsafe fn register(start: *const u8, len: usize) -> RootSpan {
        let start = start as usize;
        debug_assert!(start % std::mem::size_of::<usize>() == 0);
        let span = RootSpan {
            start,
            end: start + len,
        };
        SPANS.lock().push((span.start, span.end));
        span
    }

    /// The first byte of the span, and one past its last.
    pub fn range(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

impl Drop for RootSpan {
    fn drop(&mut self) {
        let mut spans = SPANS.lock();
        if let Some(index) = spans.iter().position(|&s| s == (self.start, self.end)) {
            spans.swap_remove(index);
        }
    }
}

/// Appends every word of every registered span to `out`.
pub(crate) fn span_words(out: &mut Vec<usize>) {
    let word = std::mem::size_of::<usize>();
    for &(start, end) in SPANS.lock().iter() {
        let mut address = start;
        while address + word <= end {
            // Safety: a registered span is mapped and readable (see
            // `RootSpan::register`), and with the world stopped no thread
            // writes it.
            out.push(unsafe { std::ptr::read_volatile(address as *const usize) });
            address += word;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_is_read_in_place_until_it_is_dropped() {
        let memory: Vec<usize> = vec![0x5eed_0101, 0x5eed_0102];
        let span =
            unsafe { RootSpan::register(memory.as_ptr().cast(), 2 * std::mem::size_of::<usize>()) };
        let mut seen = Vec::new();
        collect_words(&mut seen);
        assert!(seen.contains(&0x5eed_0101) && seen.contains(&0x5eed_0102));
        drop(span);
        let mut seen = Vec::new();
        collect_words(&mut seen);
        assert!(!seen.contains(&0x5eed_0101));
    }

    #[test]
    fn a_registered_region_is_read_until_it_is_dropped() {
        let region = RootRegion::new();
        region.with(|words| words.extend([0x5eed_0001, 0x5eed_0002]));
        let mut seen = Vec::new();
        collect_words(&mut seen);
        assert!(seen.contains(&0x5eed_0001) && seen.contains(&0x5eed_0002));
        drop(region);
        let mut seen = Vec::new();
        collect_words(&mut seen);
        assert!(!seen.contains(&0x5eed_0001));
    }
}
