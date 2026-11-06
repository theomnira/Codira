//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Root regions: memory outside every stack that the collector scans
//!   conservatively, like a parked thread's stack. The task runtime keeps the
//!   results of finished tasks in one until the task that joins them has
//!   copied them out (`codira_runtime::tasks::group`).
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

/// Appends the words of every registered region to `out`. Called by the
/// collector with every mutator parked.
pub(crate) fn collect_words(out: &mut Vec<usize>) {
    let mut regions = REGIONS.lock();
    regions.retain(|weak| weak.strong_count() > 0);
    for region in regions.iter().filter_map(Weak::upgrade) {
        out.extend_from_slice(&region.words.lock());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
