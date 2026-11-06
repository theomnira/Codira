//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Root ranges: memory outside the GC heap and outside any thread's stack
//!   whose words the collector treats as roots, conservatively, while the
//!   range is registered. Region arenas (`gc::region`) register each chunk
//!   they hold, so a GC reference stored in a region object keeps its heap
//!   object alive.
//!
//! # How the collector uses them
//!
//! A collection stops the world, reads every word of every parked thread's
//! stack (`gc::safepoint`, `gc::stack_scan`) and then every word of every
//! registered range, and treats each word that points into a live heap
//! object as a root, exactly as it treats a stack word. A word that points
//! anywhere else -- into a range, into a stack -- is ignored. Nothing in a
//! range is traced precisely: the collector does not know the layout of
//! what is stored there.
//!
//! `gc::root_region` is the other way to add roots: a root region owns a
//! vector of words (the task runtime copies finished tasks' results into
//! one), while a root range points at memory that lives elsewhere, so that
//! an arena's objects are scanned in place without copying anything. Both
//! are read with the world stopped, after the parked stacks.
//!
//! This is deliberately separate from the stack scan. A precise collector
//! (one that finds stack roots from stack maps instead of scanning) will
//! either keep scanning registered ranges conservatively, as here -- sound,
//! because objects do not move -- or need maps of the reference slots of
//! the objects stored in them, which a region arena could provide from the
//! types it allocates. A moving collector could not keep the conservative
//! scan: it cannot update a word it is not sure is a reference.
//!
//! # Synchronization
//!
//! The registry is a mutex-protected map. A thread registers a range before
//! it stores anything in it and unregisters it before it frees the memory.
//! Neither operation is a safepoint, so no thread is ever parked while it
//! holds the lock, and the collector, which takes the lock with the world
//! stopped, never waits for a parked thread.

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

/// The ranges registered with one heap.
#[derive(Default)]
pub(crate) struct RootRanges {
    /// Start address to end address (exclusive).
    ranges: Mutex<FxHashMap<usize, usize>>,
}

impl RootRanges {
    /// Registers `[start, end)`.
    ///
    /// # Safety
    ///
    /// The memory must be readable, and stay mapped, until the range is
    /// unregistered; `start` must be word-aligned.
    pub(crate) unsafe fn register(&self, start: usize, end: usize) {
        debug_assert!(start % std::mem::size_of::<usize>() == 0);
        let previous = self.ranges.lock().insert(start, end);
        debug_assert!(previous.is_none(), "root range registered twice");
    }

    /// Unregisters the range that starts at `start`.
    pub(crate) fn unregister(&self, start: usize) {
        let removed = self.ranges.lock().remove(&start);
        debug_assert!(removed.is_some(), "unregistering an unknown root range");
    }

    /// Every registered range.
    pub(crate) fn ranges(&self) -> Vec<(usize, usize)> {
        self.ranges
            .lock()
            .iter()
            .map(|(&start, &end)| (start, end))
            .collect()
    }

    /// Appends every word of every registered range to `words`.
    pub(crate) fn extend_words(&self, words: &mut Vec<usize>) {
        let word = std::mem::size_of::<usize>();
        for (start, end) in self.ranges() {
            let mut address = start;
            while address + word <= end {
                // Safety: a registered range is mapped and readable (see
                // `register`), and with the world stopped no thread writes
                // it.
                words.push(unsafe { std::ptr::read_volatile(address as *const usize) });
                address += word;
            }
        }
    }

    /// The number of registered ranges and their total size in bytes.
    pub(crate) fn totals(&self) -> (usize, usize) {
        let ranges = self.ranges.lock();
        (
            ranges.len(),
            ranges.iter().map(|(start, end)| end - start).sum(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_words_are_read_and_unregistered_ones_are_not() {
        let ranges = RootRanges::default();
        let memory: Vec<usize> = vec![1, 2, 3, 4];
        let start = memory.as_ptr() as usize;
        let end = start + 4 * std::mem::size_of::<usize>();
        unsafe { ranges.register(start, end) };
        let mut words = Vec::new();
        ranges.extend_words(&mut words);
        assert_eq!(words, vec![1, 2, 3, 4]);
        assert_eq!(ranges.totals(), (1, 4 * std::mem::size_of::<usize>()));
        ranges.unregister(start);
        let mut words = Vec::new();
        ranges.extend_words(&mut words);
        assert!(words.is_empty());
        assert_eq!(ranges.totals(), (0, 0));
    }
}
