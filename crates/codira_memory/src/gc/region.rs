//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Region arenas for `@region` functions: objects with the heap's layout
//!   (a header whose first word points to the payload) bump-allocated in
//!   chunks outside the GC heap, all freed together when the region closes.
//! - Every chunk is a registered root range (`gc::root_ranges`) while the
//!   region is open, so the GC references region objects hold keep their heap
//!   objects alive.
//! - An optional check at close ([`MarkSweep::set_region_checks`], on in
//!   debug builds and with `CODIRA_CHECK_REGIONS=1`): with the world stopped,
//!   no heap object and no other open region refers into the closing
//!   region's chunks.
//!
//! # What makes a region sound
//!
//! The compiler only places an object in a region when its escape analysis
//! (`codira_hir::escape`) proves that no reference to it survives the call
//! that opened the region, and that no heap object ever holds one. So when
//! the region closes, its objects are unreachable from everything but the
//! dead frame that made them, and freeing the chunks is safe. The collector
//! never traces a region object (no heap object refers to one; a word in a
//! stack or a root range that points into a chunk is not a heap address and
//! is ignored), and the heap objects a region object refers to are roots
//! through the chunk's root range.
//!
//! A region object is never seen by the runtime: hot reload migrates the
//! heap only while no Codira frame is live (so no region is open), and a heap
//! snapshot writes heap objects only, none of which refers to a region.

use std::{
    alloc::{alloc_zeroed, dealloc, Layout},
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use crate::gc::{
    heap::ObjectHeader, mark_sweep::for_each_reference, safepoint, Event, GcPtr, MarkSweep,
    Observer,
};

/// The size of an ordinary chunk. An allocation larger than a chunk gets a
/// chunk of its own.
const CHUNK_SIZE: usize = 64 * 1024;

/// The header every region object starts with, as on the heap.
const HEADER: Layout = Layout::new::<ObjectHeader>();

struct Chunk {
    start: NonNull<u8>,
    layout: Layout,
}

/// An open region: the chunks it allocated from, and where the next
/// allocation goes.
pub struct Region {
    chunks: Vec<Chunk>,
    /// The next free byte of the last ordinary chunk, and its end.
    cursor: usize,
    end: usize,
}

/// What regions did since the heap was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RegionStats {
    /// Regions opened and closed.
    pub opened: u64,
    pub closed: u64,
    /// Objects allocated in regions, and their bytes (headers and padding
    /// included).
    pub objects_allocated: u64,
    pub bytes_allocated: u64,
    /// Chunks allocated, and the bytes they held.
    pub chunks_allocated: u64,
    pub chunk_bytes_allocated: u64,
    /// Root ranges registered now (one per chunk of every open region), and
    /// their bytes.
    pub live_ranges: usize,
    pub live_range_bytes: usize,
}

/// The counters behind [`RegionStats`].
#[derive(Default)]
pub(crate) struct RegionCounters {
    opened: AtomicU64,
    closed: AtomicU64,
    objects: AtomicU64,
    bytes: AtomicU64,
    chunks: AtomicU64,
    chunk_bytes: AtomicU64,
    /// Whether closing a region checks that nothing refers into it.
    check: AtomicBool,
}

impl RegionCounters {
    pub(crate) fn new() -> Self {
        let counters = RegionCounters::default();
        let check = cfg!(debug_assertions)
            || std::env::var_os("CODIRA_CHECK_REGIONS").is_some_and(|v| v != "0");
        counters.check.store(check, Ordering::Relaxed);
        counters
    }
}

/// A closing region is still referred to.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a closed region is still referred to: {0}")]
pub struct RegionLeak(pub String);

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    /// Opens a region. It allocates nothing until its first object.
    pub fn region_open(&self) -> Box<Region> {
        self.core.regions.opened.fetch_add(1, Ordering::Relaxed);
        Box::new(Region {
            chunks: Vec::new(),
            cursor: 0,
            end: 0,
        })
    }

    /// Allocates an object with a zeroed payload of `payload` in `region`
    /// and returns its handle: the address of a header whose first word
    /// points to the payload. Not a safepoint.
    ///
    /// Aborts the process when the system is out of memory, as a heap
    /// allocation does.
    pub fn region_alloc(&self, region: &mut Region, payload: Layout) -> GcPtr {
        let (layout, offset) = HEADER
            .extend(payload)
            .expect("a region object's layout overflows");
        let layout = layout.pad_to_align();
        let counters = &self.core.regions;
        let fitting = align_up(region.cursor, layout.align())
            .filter(|&start| region.cursor != 0 && start + layout.size() <= region.end);
        let start = if let Some(start) = fitting {
            region.cursor = start + layout.size();
            start
        } else if layout.size() > CHUNK_SIZE / 4 {
            // A chunk of its own, so the current chunk keeps its free space.
            self.region_chunk(region, layout.size(), layout.align())
                .as_ptr() as usize
        } else {
            let chunk = self.region_chunk(region, CHUNK_SIZE, layout.align());
            region.cursor = chunk.as_ptr() as usize + layout.size();
            region.end = chunk.as_ptr() as usize + CHUNK_SIZE;
            chunk.as_ptr() as usize
        };
        counters.objects.fetch_add(1, Ordering::Relaxed);
        counters
            .bytes
            .fetch_add(layout.size() as u64, Ordering::Relaxed);
        let header = start as *mut ObjectHeader;
        // Safety: `start..start + layout.size()` is zeroed memory of an open
        // chunk, aligned for the header. Only `data` is set: the rest of the
        // header is read by the runtime alone, which never sees a region
        // object.
        unsafe {
            (*header).data = (start + offset) as *mut u8;
        }
        (header as crate::gc::RawGcPtr).into()
    }

    /// A new zeroed chunk of `size` bytes, registered as a root range.
    fn region_chunk(&self, region: &mut Region, size: usize, align: usize) -> NonNull<u8> {
        let layout = Layout::from_size_align(size, align.max(16)).expect("a region chunk's layout");
        // Safety: the layout has a non-zero size.
        let Some(start) = NonNull::new(unsafe { alloc_zeroed(layout) }) else {
            std::alloc::handle_alloc_error(layout)
        };
        // Safety: the chunk stays allocated until `region_close`
        // unregisters it.
        unsafe {
            self.core
                .root_ranges
                .register(start.as_ptr() as usize, start.as_ptr() as usize + size)
        };
        region.chunks.push(Chunk { start, layout });
        let counters = &self.core.regions;
        counters.chunks.fetch_add(1, Ordering::Relaxed);
        counters
            .chunk_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
        start
    }

    /// Closes `region`: unregisters and frees every chunk. With region checks
    /// on, first stops the world and makes sure no heap object and no other
    /// open region refers into the chunks.
    pub fn region_close(&self, region: Box<Region>) -> Result<(), RegionLeak> {
        for chunk in &region.chunks {
            self.core
                .root_ranges
                .unregister(chunk.start.as_ptr() as usize);
        }
        let checked =
            if self.core.regions.check.load(Ordering::Relaxed) && !region.chunks.is_empty() {
                self.check_unreferenced(&region)
            } else {
                Ok(())
            };
        for chunk in &region.chunks {
            // Safety: allocated by `region_chunk` with this layout, and no
            // longer registered.
            unsafe { dealloc(chunk.start.as_ptr(), chunk.layout) };
        }
        self.core.regions.closed.fetch_add(1, Ordering::Relaxed);
        checked
    }

    /// Whether region checks are on (see [`MarkSweep::set_region_checks`]).
    pub fn region_checks(&self) -> bool {
        self.core.regions.check.load(Ordering::Relaxed)
    }

    /// Turns the check at every region's close on or off. On by default in
    /// debug builds and when `CODIRA_CHECK_REGIONS` is set (to anything but
    /// `0`). The check stops the world and reads every heap object's
    /// references and every other open region's chunks, so it costs a
    /// stop-the-world pause per closed region.
    pub fn set_region_checks(&self, enabled: bool) {
        self.core.regions.check.store(enabled, Ordering::Relaxed);
    }

    /// What regions did since the heap was created.
    pub fn region_stats(&self) -> RegionStats {
        let counters = &self.core.regions;
        let (live_ranges, live_range_bytes) = self.core.root_ranges.totals();
        RegionStats {
            opened: counters.opened.load(Ordering::Relaxed),
            closed: counters.closed.load(Ordering::Relaxed),
            objects_allocated: counters.objects.load(Ordering::Relaxed),
            bytes_allocated: counters.bytes.load(Ordering::Relaxed),
            chunks_allocated: counters.chunks.load(Ordering::Relaxed),
            chunk_bytes_allocated: counters.chunk_bytes.load(Ordering::Relaxed),
            live_ranges,
            live_range_bytes,
        }
    }

    /// The check behind [`MarkSweep::region_close`]: every reference slot
    /// of every heap object (reachable or not: a heap object never holds a
    /// region reference, dead or alive), and every word of every other
    /// registered root range, must point outside `region`'s chunks.
    fn check_unreferenced(&self, region: &Region) -> Result<(), RegionLeak> {
        let chunks: Vec<(usize, usize)> = region
            .chunks
            .iter()
            .map(|c| {
                let start = c.start.as_ptr() as usize;
                (start, start + c.layout.size())
            })
            .collect();
        let inside = |word: usize| chunks.iter().any(|&(s, e)| word >= s && word < e);
        let check = || {
            let inner = self.heap.inner.read();
            let mut found = None;
            inner.for_each_object(|object| {
                if found.is_some() {
                    return;
                }
                // Safety: the world is stopped, so the object's payload
                // matches its type and nothing writes it.
                unsafe {
                    for_each_reference(object.as_ref(), |child| {
                        let slot = child.as_ptr() as usize;
                        if found.is_none() && inside(slot) {
                            found = Some(format!(
                                "a heap object of type `{}` holds the address {slot:#x}",
                                object.as_ref().type_view().name()
                            ));
                        }
                    });
                }
            });
            drop(inner);
            if found.is_none() {
                let mut words = Vec::new();
                self.core.root_ranges.extend_words(&mut words);
                if let Some(word) = words.into_iter().find(|&w| inside(w)) {
                    found = Some(format!("another open region holds the address {word:#x}"));
                }
            }
            found
        };
        let found = if safepoint::holds_stop() {
            check()
        } else {
            safepoint::stop_the_world(|_| check())
        };
        match found {
            Some(what) => Err(RegionLeak(what)),
            None => Ok(()),
        }
    }
}

fn align_up(address: usize, align: usize) -> Option<usize> {
    address.checked_add(align - 1).map(|a| a & !(align - 1))
}

#[cfg(test)]
mod tests {
    use std::alloc::Layout;

    use crate::gc::{GcPtr, MarkSweep, NoopObserver};

    fn payload(handle: GcPtr) -> *mut usize {
        // The first word of a header is the payload address.
        unsafe { *(handle.as_ptr() as *const *mut usize) }
    }

    #[test]
    fn objects_are_zeroed_aligned_and_counted() {
        let gc = MarkSweep::<NoopObserver<_>>::default();
        let mut region = gc.region_open();
        let mut seen = Vec::new();
        for i in 0..10_000usize {
            let handle = gc.region_alloc(&mut region, Layout::from_size_align(24, 8).unwrap());
            let data = payload(handle);
            assert_eq!(handle.as_ptr() as usize % 16, 0);
            assert_eq!(unsafe { *data }, 0);
            unsafe { *data = i };
            seen.push(data);
        }
        // A large object gets a chunk of its own.
        let big = gc.region_alloc(&mut region, Layout::from_size_align(1 << 20, 64).unwrap());
        assert_eq!(payload(big) as usize % 64, 0);
        for (i, data) in seen.iter().enumerate() {
            assert_eq!(unsafe { **data }, i, "objects overlap");
        }
        let stats = gc.region_stats();
        assert_eq!(stats.objects_allocated, 10_001);
        assert!(stats.live_ranges >= 2);
        gc.region_close(region).unwrap();
        let stats = gc.region_stats();
        assert_eq!((stats.opened, stats.closed), (1, 1));
        assert_eq!((stats.live_ranges, stats.live_range_bytes), (0, 0));
    }
}
