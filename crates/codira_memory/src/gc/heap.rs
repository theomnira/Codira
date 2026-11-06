//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Segregated size-class heap backing the mark-sweep collector.
//!
//! Every GC object is a single *cell*: a 32-byte [`ObjectHeader`] followed by
//! the object's payload. The header's first word holds a pointer to the
//! payload, which is what generated code dereferences, so a [`GcPtr`] is the
//! address of the header and keeps the double-pointer shape hot reload relies
//! on: migration can point `data` somewhere else while the handle stays put.
//!
//! Cells up to [`MAX_SMALL_CELL`] bytes come from 64 KiB blocks, each block
//! carved into equally sized cells of one size class. Each thread keeps, per
//! heap and per size class, a *run* of contiguous free cells it leased from
//! the heap; allocating is a bump within that run and never takes the heap
//! lock. Only refilling an exhausted run, allocating a large object, and the
//! collector take the lock. Larger objects are individually allocated.
//!
//! The heap never moves a header, so handles stay valid for the lifetime of
//! the object. Objects are enumerated for sweeping and migration by walking
//! blocks; there is no per-object map on the allocation path.
//!
//! Concurrency contract: allocation, rooting and reading types are
//! thread-safe. A collection or a memory migration stops the world first
//! (`gc::safepoint`), because generated code holds unrooted handles on its
//! stack that only a conservative scan of a parked thread can find.

use std::{
    alloc::Layout,
    cell::RefCell,
    ptr::NonNull,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc, Weak,
    },
};

use parking_lot::RwLock;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    gc::{array::ArrayHeader, GcPtr, RawGcPtr},
    r#type::{Type, TypeData, TypeView},
};

/// Size of an object header. Payloads start this many bytes after a cell.
pub(crate) const HEADER_SIZE: usize = std::mem::size_of::<ObjectHeader>();

/// Size of a block of small cells.
pub(crate) const BLOCK_SIZE: usize = 64 * 1024;

/// Alignment of a block. Cell sizes are multiples of 16, so every payload is
/// at least 16-byte aligned.
const BLOCK_ALIGN: usize = 64;

/// The largest payload alignment a small cell can satisfy.
const SMALL_PAYLOAD_ALIGN: usize = 16;

/// The largest cell (header + payload) served from a size class.
pub(crate) const MAX_SMALL_CELL: usize = 2048;

/// Cell sizes (header included) of every size class.
const CLASS_SIZES: [u32; 23] = [
    32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024,
    1280, 1536, 1792, 2048,
];

/// The number of size classes.
pub(crate) const NUM_CLASSES: usize = CLASS_SIZES.len();

/// The `class` value of an individually allocated (large) object.
pub(crate) const LARGE_CLASS: u8 = u8::MAX;

/// Maps `(cell_size + 15) / 16` to the smallest size class that fits.
static CLASS_OF: [u8; MAX_SMALL_CELL / 16 + 1] = build_class_table();

const fn build_class_table() -> [u8; MAX_SMALL_CELL / 16 + 1] {
    let mut table = [0u8; MAX_SMALL_CELL / 16 + 1];
    let mut index = 0;
    while index < table.len() {
        let size = (index * 16) as u32;
        let mut class = 0;
        while CLASS_SIZES[class] < size {
            class += 1;
        }
        table[index] = class as u8;
        index += 1;
    }
    table
}

/// Returns the size class for a payload with the given layout, or `None` if
/// the object must be allocated individually.
#[inline]
pub(crate) fn size_class(payload: Layout) -> Option<u8> {
    let total = HEADER_SIZE + payload.size();
    if payload.align() > SMALL_PAYLOAD_ALIGN || total > MAX_SMALL_CELL {
        None
    } else {
        Some(CLASS_OF[(total + 15) >> 4])
    }
}

/// Returns the cell size of a size class.
#[inline]
pub(crate) fn class_size(class: u8) -> usize {
    CLASS_SIZES[class as usize] as usize
}

/// Cell state: never allocated or freed by a collection.
pub(crate) const STATE_FREE: u8 = 0;
/// Cell state: holds a live (or not yet collected) object.
pub(crate) const STATE_ALLOCATED: u8 = 1;

/// Mark color: not reached by the current collection (yet).
pub(crate) const COLOR_WHITE: u8 = 0;
/// Mark color: reached by the current collection.
pub(crate) const COLOR_BLACK: u8 = 1;

/// Header flag: the payload does not live inline after the header but in a
/// separate allocation (set by hot-reload migration when the new layout no
/// longer fits the cell).
pub(crate) const FLAG_OUT_OF_LINE: u8 = 1;

/// The header that precedes every GC object.
///
/// `data` MUST be the first field: a [`GcPtr`] points at the header, and both
/// generated code and [`crate::gc::HasIndirectionPtr`] load the payload
/// address from the first word.
#[repr(C, align(16))]
pub(crate) struct ObjectHeader {
    /// Pointer to the payload (for arrays: to the [`ArrayHeader`]).
    pub data: *mut u8,
    /// The object's type. Kept alive by the heap's type registry.
    pub ty: *const TypeData,
    /// Number of times the object was rooted by the host.
    pub roots: AtomicU32,
    /// [`STATE_FREE`] or [`STATE_ALLOCATED`].
    pub state: AtomicU8,
    /// Mark color, only touched by the allocating thread and the collector.
    pub color: u8,
    /// `FLAG_*` bits.
    pub flags: u8,
    /// The size class of the cell, or [`LARGE_CLASS`].
    pub class: u8,
    /// Number of payload bytes available inline after the header.
    pub inline_capacity: usize,
}

const _: () = assert!(std::mem::size_of::<ObjectHeader>() == 32);
const _: () = assert!(std::mem::offset_of!(ObjectHeader, data) == 0);

impl ObjectHeader {
    /// Returns a borrowed view of the object's type.
    ///
    /// # Safety
    ///
    /// The header must belong to an allocated object.
    #[inline]
    pub(crate) unsafe fn type_view(&self) -> TypeView {
        Type::view(NonNull::new_unchecked(self.ty as *mut TypeData))
    }

    /// Returns the object's type data.
    ///
    /// # Safety
    ///
    /// The header must belong to an allocated object.
    #[inline]
    pub(crate) unsafe fn type_data(&self) -> &TypeData {
        &*self.ty
    }

    /// Returns the number of payload bytes this object currently occupies.
    ///
    /// # Safety
    ///
    /// The header must belong to an allocated object.
    pub(crate) unsafe fn payload_size(&self) -> usize {
        let ty = self.type_data();
        match ty.array_element() {
            Some(element) => {
                let capacity = (*self.data.cast::<ArrayHeader>()).capacity;
                array_payload_layout(element, capacity)
                    .expect("array layout overflow")
                    .size()
            }
            None => ty.layout().size(),
        }
    }
}

/// Converts a header pointer into a handle.
#[inline]
pub(crate) fn handle_of(header: NonNull<ObjectHeader>) -> GcPtr {
    (header.as_ptr() as RawGcPtr).into()
}

/// Converts a handle into a header pointer.
#[inline]
pub(crate) fn header_of(handle: GcPtr) -> NonNull<ObjectHeader> {
    NonNull::new(handle.as_ptr() as *mut ObjectHeader).expect("null GC handle")
}

/// Returns the layout of the payload of an array with `capacity` elements of
/// type `element`: an [`ArrayHeader`] followed by the (reference-sized for
/// reference types) elements.
pub(crate) fn array_payload_layout(element: &TypeData, capacity: usize) -> Option<Layout> {
    let element_layout = element.reference_layout();
    let stride = element_layout.pad_to_align().size();
    let elements =
        Layout::from_size_align(stride.checked_mul(capacity)?, element_layout.align()).ok()?;
    let (layout, _) = Layout::new::<ArrayHeader>().extend(elements).ok()?;
    Some(layout)
}

/// An error returned when the heap cannot provide memory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllocError {
    /// Committing more memory would exceed the configured heap limit.
    ///
    /// When the allocation came from a thread running Codira code, a full
    /// collection ran first (see `MarkSweep::try_alloc`), so `live` is what
    /// survived it.
    #[error(
        "GC heap limit of {limit} bytes exceeded: {requested} more bytes requested, {committed} \
         bytes committed, {live} bytes in objects not known to be garbage"
    )]
    LimitExceeded {
        /// The configured limit on committed bytes.
        limit: usize,
        /// Bytes the heap holds from the system (blocks, large objects and
        /// out-of-line payloads).
        committed: usize,
        /// Bytes the failed request needed on top of `committed`: a whole
        /// 64 KiB block for a small object, the object itself otherwise.
        requested: usize,
        /// Payload bytes of the objects the last collection found live plus
        /// everything allocated since (headers and size-class rounding not
        /// included).
        live: usize,
    },
    /// The system allocator returned null.
    #[error("the system allocator could not provide {requested} bytes")]
    SystemOutOfMemory { requested: usize },
    /// The requested object is too large to describe with a memory layout.
    #[error("the requested object is too large")]
    TooLarge,
}

/// An in-progress payload replacement, see
/// [`HeapInner::begin_payload_migration`].
pub(crate) struct PayloadMigration {
    /// The old payload (or a copy of it).
    pub src: NonNull<u8>,
    /// The new, zeroed payload the header now points to.
    pub dest: NonNull<u8>,
    /// A buffer to free once `src` is no longer needed, and whether it counts
    /// towards the committed heap size.
    release: Option<(NonNull<u8>, Layout, bool)>,
}

/// A block of equally sized cells.
struct Block {
    base: NonNull<u8>,
    class: u8,
    cell_size: usize,
    cell_count: usize,
}

impl Block {
    fn end(&self) -> usize {
        self.base.as_ptr() as usize + self.cell_size * self.cell_count
    }
}

/// A run leased to a thread cache.
#[derive(Clone, Copy, Debug)]
struct Lease {
    end: usize,
    class: u8,
    sweeps: u64,
}

/// A range of contiguous free cells `[start, end)` of one size class.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Run {
    start: usize,
    end: usize,
}

/// The heap state protected by the heap lock.
pub(crate) struct HeapInner {
    /// All blocks of small cells, of every size class.
    blocks: Vec<Block>,
    /// Free runs per size class, rebuilt by every sweep.
    free_runs: [Vec<Run>; NUM_CLASSES],
    /// Runs currently leased to a thread cache, keyed by run start address.
    /// The value is the run end, the run's size class, and the number of
    /// sweeps that had completed when the run was leased.
    leases: FxHashMap<usize, Lease>,
    /// The number of sweeps performed so far.
    sweeps: u64,
    /// Individually allocated objects: header address -> allocation layout.
    large: FxHashMap<usize, Layout>,
    /// Out-of-line payloads created by migration: header address -> (payload,
    /// layout).
    out_of_line: FxHashMap<usize, (NonNull<u8>, Layout)>,
    /// Owning references to the type of every object that was allocated in
    /// this heap, keyed by type data address. Headers store raw pointers.
    types: FxHashMap<usize, Type>,
    /// Allocation counters of the thread caches of this heap.
    thread_counters: Vec<Arc<ThreadCounters>>,
    /// Live payload bytes not accounted for by thread counters. Can be
    /// negative when objects counted by a thread were freed.
    global_bytes: isize,
    /// Empty blocks kept for reuse instead of returning them to the system.
    empty_blocks: Vec<NonNull<u8>>,
    /// Bytes committed by blocks (including cached empty blocks), large
    /// objects and out-of-line payloads.
    committed: usize,
    /// Optional cap on `committed`.
    limit: Option<usize>,
    /// The most `committed` has ever been.
    peak_committed: usize,
    /// Payload bytes and objects allocated through the locked paths, ever.
    locked_bytes_total: u64,
    locked_objects_total: u64,
    /// Payload bytes and objects allocated by thread caches that no longer
    /// exist.
    retired_bytes_total: u64,
    retired_objects_total: u64,
    /// Live payload bytes the last sweep found.
    live_after_sweep: usize,
    /// Pacing (see [`crate::gc::GcPolicy`]): grow by this percentage over
    /// `live_after_sweep` before collecting; `None` collects only at the
    /// limit.
    growth_percent: Option<u32>,
    /// Pacing never asks for a collection below this many live bytes.
    min_heap: usize,
    /// Pacing asks for a collection once [`HeapInner::allocated_bytes`] is
    /// above this; `usize::MAX` when pacing is off.
    trigger: usize,
    /// Set when `trigger` was passed: the next allocation by a thread
    /// running Codira code collects. Shared with [`Heap::due`].
    due: Arc<AtomicBool>,
}

/// What a sweep freed.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SweepOutcome {
    /// Objects freed.
    pub objects: usize,
    /// Payload bytes of the objects freed (headers and size-class rounding
    /// not included, as in [`HeapInner::allocated_bytes`]).
    pub bytes: usize,
}

/// A consistent reading of the heap's counters, for statistics.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HeapCounters {
    pub committed: usize,
    pub peak_committed: usize,
    pub limit: Option<usize>,
    pub live: usize,
    pub live_after_sweep: usize,
    pub trigger: Option<usize>,
    pub bytes_allocated: u64,
    pub objects_allocated: u64,
}

/// Allocation counters of one thread cache. Only the owning thread writes
/// them; they only grow.
#[derive(Default)]
pub(crate) struct ThreadCounters {
    /// Payload bytes allocated.
    bytes: AtomicUsize,
    /// Objects allocated.
    objects: AtomicUsize,
}

/// The maximum number of empty blocks a heap keeps cached.
const MAX_CACHED_EMPTY_BLOCKS: usize = 16;

fn block_layout() -> Layout {
    Layout::from_size_align(BLOCK_SIZE, BLOCK_ALIGN).expect("invalid block layout")
}

impl HeapInner {
    fn new(limit: Option<usize>, due: Arc<AtomicBool>) -> Self {
        Self {
            blocks: Vec::new(),
            free_runs: Default::default(),
            leases: FxHashMap::default(),
            sweeps: 0,
            large: FxHashMap::default(),
            out_of_line: FxHashMap::default(),
            types: FxHashMap::default(),
            thread_counters: Vec::new(),
            global_bytes: 0,
            empty_blocks: Vec::new(),
            committed: 0,
            limit,
            peak_committed: 0,
            locked_bytes_total: 0,
            locked_objects_total: 0,
            retired_bytes_total: 0,
            retired_objects_total: 0,
            live_after_sweep: 0,
            growth_percent: None,
            min_heap: 0,
            trigger: usize::MAX,
            due,
        }
    }

    /// Records `bytes` more committed memory.
    #[inline]
    fn add_committed(&mut self, bytes: usize) {
        self.committed += bytes;
        self.peak_committed = self.peak_committed.max(self.committed);
    }

    /// Sets the pacing parameters (see [`crate::gc::GcPolicy`]) and
    /// recomputes when the next paced collection is due.
    pub(crate) fn set_pacing(&mut self, growth_percent: Option<u32>, min_heap: usize) {
        self.growth_percent = growth_percent;
        self.min_heap = min_heap;
        self.recompute_trigger();
        self.check_trigger();
    }

    fn recompute_trigger(&mut self) {
        self.trigger = match self.growth_percent {
            None => usize::MAX,
            Some(percent) => {
                let goal = self.live_after_sweep as u128 * (100 + u128::from(percent)) / 100;
                usize::try_from(goal)
                    .unwrap_or(usize::MAX)
                    .max(self.min_heap)
            }
        };
    }

    /// Asks for a collection if pacing's trigger was reached. Called on the
    /// slow allocation paths (refilling a thread's run, large objects,
    /// growing arrays), so the per-object fast path pays nothing for it.
    #[inline]
    fn check_trigger(&self) {
        if self.trigger != usize::MAX && self.allocated_bytes() > self.trigger {
            self.due.store(true, Ordering::Relaxed);
        }
    }

    /// Returns the heap's counters.
    pub(crate) fn counters(&self) -> HeapCounters {
        let (thread_bytes, thread_objects) =
            self.thread_counters.iter().fold((0u64, 0u64), |(b, o), c| {
                (
                    b + c.bytes.load(Ordering::Relaxed) as u64,
                    o + c.objects.load(Ordering::Relaxed) as u64,
                )
            });
        HeapCounters {
            committed: self.committed,
            peak_committed: self.peak_committed,
            limit: self.limit,
            live: self.allocated_bytes(),
            live_after_sweep: self.live_after_sweep,
            trigger: (self.trigger != usize::MAX).then_some(self.trigger),
            bytes_allocated: self.locked_bytes_total + self.retired_bytes_total + thread_bytes,
            objects_allocated: self.locked_objects_total
                + self.retired_objects_total
                + thread_objects,
        }
    }

    /// Checks that `requested` more bytes can be committed.
    fn reserve(&mut self, requested: usize) -> Result<(), AllocError> {
        let Some(limit) = self.limit else {
            return Ok(());
        };
        if self.committed.saturating_add(requested) > limit {
            // Cached empty blocks count towards the limit but cannot serve
            // this request (they would have been used already if they could),
            // so give them back first.
            while self.committed.saturating_add(requested) > limit {
                let Some(base) = self.empty_blocks.pop() else {
                    break;
                };
                unsafe { std::alloc::dealloc(base.as_ptr(), block_layout()) };
                self.committed -= BLOCK_SIZE;
            }
        }
        if self.committed.saturating_add(requested) > limit {
            Err(AllocError::LimitExceeded {
                limit,
                committed: self.committed,
                requested,
                live: self.allocated_bytes(),
            })
        } else {
            Ok(())
        }
    }

    /// Returns every allocated object that one of `words` points into: at its
    /// header, anywhere inside its cell or payload (including an out-of-line
    /// payload), or one byte past its end. Used as conservative roots when
    /// the collector runs during an allocation.
    pub(crate) fn objects_referenced_by(&self, words: &mut [usize]) -> Vec<NonNull<ObjectHeader>> {
        #[derive(Clone, Copy)]
        enum Kind {
            Cells(usize),
            Large,
            OutOfLine(usize),
        }
        let mut ranges: Vec<(usize, usize, Kind)> = self
            .blocks
            .iter()
            .map(|b| (b.base.as_ptr() as usize, b.end(), Kind::Cells(b.cell_size)))
            .chain(
                self.large
                    .iter()
                    .map(|(&address, layout)| (address, address + layout.size(), Kind::Large)),
            )
            .chain(self.out_of_line.iter().map(|(&header, (buffer, layout))| {
                let start = buffer.as_ptr() as usize;
                (start, start + layout.size(), Kind::OutOfLine(header))
            }))
            .collect();
        if ranges.is_empty() {
            return Vec::new();
        }
        ranges.sort_unstable_by_key(|r| r.0);
        let lowest = ranges[0].0;
        let highest = ranges.iter().map(|r| r.1).max().unwrap_or(0);

        words.sort_unstable();
        let mut found = Vec::new();
        let mut previous = None;
        for &word in words.iter() {
            if previous == Some(word) || word < lowest || word > highest {
                continue;
            }
            previous = Some(word);
            // `word - 1` catches pointers one past the end of an object, which
            // optimized loops over arrays can hold.
            for candidate in [word, word.wrapping_sub(1)] {
                let index = ranges.partition_point(|r| r.0 <= candidate);
                if index == 0 {
                    continue;
                }
                let (start, end, kind) = ranges[index - 1];
                if candidate >= end {
                    continue;
                }
                let header = match kind {
                    Kind::Cells(cell_size) => {
                        let cell = start + (candidate - start) / cell_size * cell_size;
                        let state = unsafe {
                            (*(cell as *const ObjectHeader))
                                .state
                                .load(Ordering::Acquire)
                        };
                        if state != STATE_ALLOCATED {
                            continue;
                        }
                        cell
                    }
                    Kind::Large => start,
                    Kind::OutOfLine(header) => header,
                };
                found.push(unsafe { NonNull::new_unchecked(header as *mut ObjectHeader) });
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// Returns the configured limit on committed bytes.
    pub(crate) fn limit(&self) -> Option<usize> {
        self.limit
    }

    /// Sets the limit on committed bytes. Memory already committed beyond a
    /// lower limit is not released; further growth fails until it is.
    pub(crate) fn set_limit(&mut self, limit: Option<usize>) {
        self.limit = limit;
    }

    /// Ensures the type is owned by the registry.
    pub(crate) fn register_type(&mut self, ty: &Type) {
        debug_assert!(ty.is_in_global_store());
        self.types
            .entry(ty.data_ptr().as_ptr() as usize)
            .or_insert_with(|| ty.clone());
    }

    /// Takes a free run of `class`, carving a new block if none is left.
    fn take_run(&mut self, class: u8) -> Result<Run, AllocError> {
        if let Some(run) = self.free_runs[class as usize].pop() {
            return Ok(run);
        }

        let base = if let Some(base) = self.empty_blocks.pop() {
            // A cached block may have been carved for a different class, so
            // stale state bytes could sit where the new cells' headers go.
            unsafe { std::ptr::write_bytes(base.as_ptr(), 0, BLOCK_SIZE) };
            base
        } else {
            self.reserve(BLOCK_SIZE)?;
            let base = NonNull::new(unsafe { std::alloc::alloc_zeroed(block_layout()) }).ok_or(
                AllocError::SystemOutOfMemory {
                    requested: BLOCK_SIZE,
                },
            )?;
            self.add_committed(BLOCK_SIZE);
            base
        };

        let cell_size = class_size(class);
        let block = Block {
            base,
            class,
            cell_size,
            cell_count: BLOCK_SIZE / cell_size,
        };
        let run = Run {
            start: base.as_ptr() as usize,
            end: block.end(),
        };
        self.blocks.push(block);
        Ok(run)
    }

    /// Leases a run of `class` to a thread cache.
    fn lease_run(&mut self, class: u8) -> Result<Run, AllocError> {
        let run = self.take_run(class)?;
        self.check_trigger();
        self.leases.insert(
            run.start,
            Lease {
                end: run.end,
                class,
                sweeps: self.sweeps,
            },
        );
        Ok(run)
    }

    /// Ends the lease of a thread-cache run. The unused tail `[cur, end)` is
    /// immediately available to other allocations, and so are cells in the
    /// used part that a sweep freed while the run was leased (a sweep skips
    /// leased cells, so they would otherwise stay unused until the next one).
    fn end_lease(&mut self, start: usize, cur: usize, end: usize) {
        let Some(lease) = self.leases.remove(&start) else {
            return;
        };
        let class = lease.class as usize;
        if lease.sweeps != self.sweeps {
            let cell_size = class_size(lease.class);
            let mut run_start = None;
            let mut cell = start;
            while cell < cur {
                let header = cell as *const ObjectHeader;
                let free = unsafe { (*header).state.load(Ordering::Acquire) } == STATE_FREE;
                if free {
                    run_start.get_or_insert(cell);
                } else if let Some(run_start) = run_start.take() {
                    self.free_runs[class].push(Run {
                        start: run_start,
                        end: cell,
                    });
                }
                cell += cell_size;
            }
            if let Some(run_start) = run_start {
                // Merge with the unused tail.
                self.free_runs[class].push(Run {
                    start: run_start,
                    end,
                });
                return;
            }
        }
        if cur < end {
            self.free_runs[class].push(Run { start: cur, end });
        }
    }

    /// Allocates a cell of `class` while holding the heap lock.
    fn alloc_cell_locked(&mut self, class: u8) -> Result<NonNull<ObjectHeader>, AllocError> {
        let mut run = self.take_run(class)?;
        let cell = run.start;
        run.start += class_size(class);
        if run.start < run.end {
            self.free_runs[class as usize].push(run);
        }
        Ok(unsafe { NonNull::new_unchecked(cell as *mut ObjectHeader) })
    }

    /// Allocates an individually managed object with a payload of `layout`.
    fn alloc_large_locked(&mut self, payload: Layout) -> Result<NonNull<ObjectHeader>, AllocError> {
        let align = payload.align().max(std::mem::align_of::<ObjectHeader>());
        let offset = HEADER_SIZE.next_multiple_of(payload.align());
        let size = offset
            .checked_add(payload.size())
            .ok_or(AllocError::TooLarge)?;
        let layout =
            Layout::from_size_align(size, align).map_err(|_layout_error| AllocError::TooLarge)?;
        self.reserve(layout.size())?;
        let base = NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(
            AllocError::SystemOutOfMemory {
                requested: layout.size(),
            },
        )?;
        self.add_committed(layout.size());
        self.large.insert(base.as_ptr() as usize, layout);

        let header = base.cast::<ObjectHeader>();
        unsafe {
            (*header.as_ptr()).data = base.as_ptr().add(offset);
            (*header.as_ptr()).inline_capacity = payload.size();
        }
        Ok(header)
    }

    /// Allocates an object of type `ty` with a payload of `payload` bytes
    /// while holding the heap lock. The payload is zeroed.
    pub(crate) fn alloc_locked(
        &mut self,
        ty: &Type,
        payload: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        self.register_type(ty);
        let header = if let Some(class) = size_class(payload) {
            let header = self.alloc_cell_locked(class)?;
            unsafe {
                let payload_ptr = header.as_ptr().cast::<u8>().add(HEADER_SIZE);
                std::ptr::write_bytes(payload_ptr, 0, payload.size());
                (*header.as_ptr()).data = payload_ptr;
                (*header.as_ptr()).class = class;
                (*header.as_ptr()).inline_capacity = class_size(class) - HEADER_SIZE;
            }
            header
        } else {
            let header = self.alloc_large_locked(payload)?;
            unsafe { (*header.as_ptr()).class = LARGE_CLASS };
            header
        };
        unsafe { init_header(header, ty, (*header.as_ptr()).class) };
        self.global_bytes += payload.size() as isize;
        self.locked_bytes_total += payload.size() as u64;
        self.locked_objects_total += 1;
        self.check_trigger();
        Ok(header)
    }

    /// Starts replacing the payload of `header` with a zeroed payload of
    /// `new_layout`, for hot-reload migration.
    ///
    /// The returned [`PayloadMigration`] gives a readable copy of the old
    /// payload (`src`) and the new, zeroed payload (`dest`), which the header
    /// already points to. The new payload stays inline when it fits the cell;
    /// otherwise it is allocated out of line. Call
    /// [`HeapInner::finish_payload_migration`] once `src` has been read.
    ///
    /// # Safety
    ///
    /// `header` must be an allocated object of this heap whose `ty` still
    /// describes the old payload.
    pub(crate) unsafe fn begin_payload_migration(
        &mut self,
        header: NonNull<ObjectHeader>,
        new_layout: Layout,
    ) -> Result<PayloadMigration, AllocError> {
        let hdr = header.as_ptr();
        let key = hdr as usize;
        let old_out_of_line = self.out_of_line.remove(&key);
        let fits_inline = new_layout.size() <= (*hdr).inline_capacity
            && new_layout.align() <= (*hdr).inline_payload_align();

        let (src, release) = match old_out_of_line {
            // The old payload lives in its own buffer, which stays readable
            // until the migration finishes.
            Some((buffer, layout)) => (buffer, Some((buffer, layout, true))),
            None if fits_inline => {
                // The new payload reuses the inline area, so copy the old one
                // out first.
                let old_size = (*hdr).payload_size();
                let temp_layout = Layout::from_size_align(old_size.max(1), SMALL_PAYLOAD_ALIGN)
                    .map_err(|_layout_error| AllocError::TooLarge)?;
                let temp = NonNull::new(std::alloc::alloc(temp_layout)).ok_or(
                    AllocError::SystemOutOfMemory {
                        requested: temp_layout.size(),
                    },
                )?;
                std::ptr::copy_nonoverlapping((*hdr).data, temp.as_ptr(), old_size);
                (temp, Some((temp, temp_layout, false)))
            }
            None => (NonNull::new_unchecked((*hdr).data), None),
        };

        let dest = if fits_inline {
            let inline = (*hdr).inline_payload_for(self);
            std::ptr::write_bytes(inline.as_ptr(), 0, new_layout.size());
            (*hdr).flags &= !FLAG_OUT_OF_LINE;
            inline
        } else {
            let layout = Layout::from_size_align(
                new_layout.size().max(1),
                new_layout.align().max(SMALL_PAYLOAD_ALIGN),
            )
            .map_err(|_layout_error| AllocError::TooLarge)?;
            self.reserve(layout.size())?;
            let buffer = NonNull::new(std::alloc::alloc_zeroed(layout)).ok_or(
                AllocError::SystemOutOfMemory {
                    requested: layout.size(),
                },
            )?;
            self.add_committed(layout.size());
            self.out_of_line.insert(key, (buffer, layout));
            (*hdr).flags |= FLAG_OUT_OF_LINE;
            buffer
        };
        (*hdr).data = dest.as_ptr();

        Ok(PayloadMigration { src, dest, release })
    }

    /// Moves the payload of the array `header` to a new out-of-line buffer
    /// with room for `new_capacity` elements, for a growing array (`push`,
    /// `reserve`, ...).
    ///
    /// The array header and the `length` elements in use are copied (an
    /// out-of-line buffer is `realloc`ed, which copies it whole or moves its
    /// pages); the slots past the length are left uninitialized, which is
    /// sound because
    /// nothing reads them (the collector, heap snapshots and hot-reload
    /// migration stop at the length, and generated code checks every index
    /// against it). An old out-of-line buffer is freed; an old inline
    /// payload stays in its cell, unused, until the object dies. The object
    /// keeps its address, so every handle to it stays valid.
    ///
    /// # Safety
    ///
    /// `header` must be an allocated array object of this heap, and no
    /// other thread may access it during the call.
    pub(crate) unsafe fn grow_array(
        &mut self,
        header: NonNull<ObjectHeader>,
        new_capacity: usize,
    ) -> Result<(), AllocError> {
        let hdr = header.as_ptr();
        let element = (*hdr)
            .type_data()
            .array_element()
            .expect("growing an object that is not an array");
        let old = (*hdr).data.cast::<ArrayHeader>();
        let length = (*old).length;
        let old_size = (*hdr).payload_size();
        if new_capacity <= (*old).capacity {
            return Ok(());
        }
        let new_layout = array_payload_layout(element, new_capacity).ok_or(AllocError::TooLarge)?;
        let used = array_payload_layout(element, length)
            .ok_or(AllocError::TooLarge)?
            .size();
        let layout = Layout::from_size_align(
            new_layout.size().max(1),
            new_layout.align().max(SMALL_PAYLOAD_ALIGN),
        )
        .map_err(|_layout_error| AllocError::TooLarge)?;
        let key = hdr as usize;
        let buffer = match self.out_of_line.get(&key).copied() {
            // Already out of line: `realloc`, which for a large buffer moves
            // pages instead of copying bytes.
            Some((previous, previous_layout)) if previous_layout.align() == layout.align() => {
                self.reserve(layout.size().saturating_sub(previous_layout.size()))?;
                let buffer = NonNull::new(std::alloc::realloc(
                    previous.as_ptr(),
                    previous_layout,
                    layout.size(),
                ))
                .ok_or(AllocError::SystemOutOfMemory {
                    requested: layout.size(),
                })?;
                self.committed -= previous_layout.size();
                self.add_committed(layout.size());
                buffer
            }
            _ => {
                self.reserve(layout.size())?;
                let buffer = NonNull::new(std::alloc::alloc(layout)).ok_or(
                    AllocError::SystemOutOfMemory {
                        requested: layout.size(),
                    },
                )?;
                std::ptr::copy_nonoverlapping((*hdr).data, buffer.as_ptr(), used);
                self.add_committed(layout.size());
                if let Some((previous, previous_layout)) = self.out_of_line.remove(&key) {
                    std::alloc::dealloc(previous.as_ptr(), previous_layout);
                    self.committed -= previous_layout.size();
                }
                buffer
            }
        };
        (*buffer.as_ptr().cast::<ArrayHeader>()).capacity = new_capacity;
        self.out_of_line.insert(key, (buffer, layout));
        (*hdr).flags |= FLAG_OUT_OF_LINE;
        (*hdr).data = buffer.as_ptr();
        self.global_bytes += new_layout.size() as isize - old_size as isize;
        self.locked_bytes_total += new_layout.size() as u64;
        self.check_trigger();
        Ok(())
    }

    /// Releases the buffers a [`PayloadMigration`] kept alive for reading.
    pub(crate) fn finish_payload_migration(&mut self, migration: PayloadMigration) {
        if let Some((buffer, layout, committed)) = migration.release {
            unsafe { std::alloc::dealloc(buffer.as_ptr(), layout) };
            if committed {
                self.committed -= layout.size();
            }
        }
    }

    /// Calls `f` for every allocated object.
    pub(crate) fn for_each_object(&self, mut f: impl FnMut(NonNull<ObjectHeader>)) {
        for block in &self.blocks {
            let mut cell = block.base.as_ptr() as usize;
            let end = block.end();
            while cell < end {
                let header = cell as *mut ObjectHeader;
                if unsafe { (*header).state.load(Ordering::Acquire) } == STATE_ALLOCATED {
                    f(unsafe { NonNull::new_unchecked(header) });
                }
                cell += block.cell_size;
            }
        }
        for &address in self.large.keys() {
            f(unsafe { NonNull::new_unchecked(address as *mut ObjectHeader) });
        }
    }

    /// Returns all allocated objects.
    pub(crate) fn objects(&self) -> Vec<NonNull<ObjectHeader>> {
        let mut objects = Vec::new();
        self.for_each_object(|o| objects.push(o));
        objects
    }

    /// Frees every unmarked object, clears the marks of the others, rebuilds
    /// the free runs, and releases empty blocks. `on_free` is called with the
    /// handle of every freed object. Returns what was freed, and sets when
    /// pacing asks for the next collection.
    pub(crate) fn sweep(&mut self, mut on_free: impl FnMut(GcPtr)) -> SweepOutcome {
        let before = self.allocated_bytes();
        let mut freed = 0usize;
        let mut live_bytes = 0usize;

        // Group the active leases by block so they can be skipped.
        let mut leases_by_block: FxHashMap<usize, Vec<(usize, usize)>> = FxHashMap::default();
        if !self.leases.is_empty() {
            for block in &self.blocks {
                let (lo, hi) = (block.base.as_ptr() as usize, block.end());
                for (&start, lease) in &self.leases {
                    if start >= lo && start < hi {
                        leases_by_block
                            .entry(lo)
                            .or_default()
                            .push((start, lease.end));
                    }
                }
            }
        }

        for runs in &mut self.free_runs {
            runs.clear();
        }

        let mut retained_blocks = Vec::with_capacity(self.blocks.len());
        let mut block_runs = Vec::new();
        for block in std::mem::take(&mut self.blocks) {
            let leases = leases_by_block
                .get(&(block.base.as_ptr() as usize))
                .map_or(&[][..], Vec::as_slice);
            let mut live_cells = 0usize;
            let mut run_start: Option<usize> = None;
            block_runs.clear();

            let mut cell = block.base.as_ptr() as usize;
            let end = block.end();
            while cell < end {
                let header = cell as *mut ObjectHeader;
                let mut is_free = true;
                unsafe {
                    if (*header).state.load(Ordering::Acquire) == STATE_ALLOCATED {
                        if (*header).color == COLOR_BLACK {
                            (*header).color = COLOR_WHITE;
                            live_cells += 1;
                            live_bytes += (*header).payload_size();
                            is_free = false;
                        } else {
                            self.free_object(NonNull::new_unchecked(header));
                            on_free(handle_of(NonNull::new_unchecked(header)));
                            freed += 1;
                        }
                    }
                }

                let leased = leases.iter().any(|&(s, e)| cell >= s && cell < e);
                if is_free && !leased {
                    run_start.get_or_insert(cell);
                } else if let Some(start) = run_start.take() {
                    block_runs.push(Run { start, end: cell });
                }
                cell += block.cell_size;
            }
            if let Some(start) = run_start.take() {
                block_runs.push(Run { start, end });
            }

            if live_cells == 0 && leases.is_empty() {
                self.release_block(block);
            } else {
                self.free_runs[block.class as usize].extend_from_slice(&block_runs);
                retained_blocks.push(block);
            }
        }
        self.blocks = retained_blocks;

        // Large objects
        let large: Vec<usize> = self.large.keys().copied().collect();
        for address in large {
            let header = address as *mut ObjectHeader;
            unsafe {
                if (*header).color == COLOR_BLACK {
                    (*header).color = COLOR_WHITE;
                    live_bytes += (*header).payload_size();
                } else {
                    self.free_object(NonNull::new_unchecked(header));
                    on_free(handle_of(NonNull::new_unchecked(header)));
                    freed += 1;
                }
            }
        }

        // Every surviving object has been counted exactly once, so the
        // thread counters can be folded into the exact total.
        let thread_bytes: usize = self
            .thread_counters
            .iter()
            .map(|c| c.bytes.load(Ordering::Relaxed))
            .sum();
        self.global_bytes = live_bytes as isize - thread_bytes as isize;
        self.sweeps += 1;
        self.live_after_sweep = live_bytes;
        self.recompute_trigger();
        self.due.store(false, Ordering::Relaxed);
        self.check_trigger();

        SweepOutcome {
            objects: freed,
            bytes: before.saturating_sub(live_bytes),
        }
    }

    /// Releases the memory of a dead object. For small cells the cell is
    /// marked free; large objects are returned to the system.
    ///
    /// # Safety
    ///
    /// `header` must be an allocated object of this heap that is not reachable.
    unsafe fn free_object(&mut self, header: NonNull<ObjectHeader>) {
        let hdr = header.as_ptr();
        if (*hdr).flags & FLAG_OUT_OF_LINE != 0 {
            if let Some((buffer, layout)) = self.out_of_line.remove(&(hdr as usize)) {
                std::alloc::dealloc(buffer.as_ptr(), layout);
                self.committed -= layout.size();
            }
        }
        if (*hdr).class == LARGE_CLASS {
            if let Some(layout) = self.large.remove(&(hdr as usize)) {
                std::alloc::dealloc(hdr.cast(), layout);
                self.committed -= layout.size();
            }
        } else {
            (*hdr).flags = 0;
            (*hdr).state.store(STATE_FREE, Ordering::Release);
        }
    }

    fn release_block(&mut self, block: Block) {
        if self.empty_blocks.len() < MAX_CACHED_EMPTY_BLOCKS {
            self.empty_blocks.push(block.base);
        } else {
            unsafe { std::alloc::dealloc(block.base.as_ptr(), block_layout()) };
            self.committed -= BLOCK_SIZE;
        }
    }

    /// Returns the exact number of payload bytes of live objects: those the
    /// last sweep kept plus everything allocated since.
    fn allocated_bytes(&self) -> usize {
        let thread_bytes: usize = self
            .thread_counters
            .iter()
            .map(|c| c.bytes.load(Ordering::Relaxed))
            .sum();
        (self.global_bytes + thread_bytes as isize).max(0) as usize
    }

    /// Drops registry entries for types no live object uses anymore.
    pub(crate) fn prune_types(&mut self) {
        let mut used: FxHashSet<usize> = FxHashSet::default();
        self.for_each_object(|o| {
            used.insert(unsafe { (*o.as_ptr()).ty } as usize);
        });
        self.types.retain(|address, _| used.contains(address));
    }

    /// Returns the bytes currently committed by the heap.
    pub(crate) fn committed(&self) -> usize {
        self.committed
    }
}

impl ObjectHeader {
    /// The alignment guaranteed for the inline payload area.
    fn inline_payload_align(&self) -> usize {
        if self.class == LARGE_CLASS {
            // Large objects are allocated with at least the alignment of the
            // header; the original payload alignment is not recorded, so be
            // conservative.
            std::mem::align_of::<ObjectHeader>()
        } else {
            SMALL_PAYLOAD_ALIGN
        }
    }

    /// Returns the inline payload area of the object.
    unsafe fn inline_payload_for(&self, heap: &HeapInner) -> NonNull<u8> {
        let header = self as *const Self as *mut u8;
        if self.class == LARGE_CLASS {
            let layout = heap.large[&(header as usize)];
            // The payload area is the tail of the allocation.
            NonNull::new_unchecked(header.add(layout.size() - self.inline_capacity))
        } else {
            NonNull::new_unchecked(header.add(HEADER_SIZE))
        }
    }
}

/// Initializes the header of a freshly taken cell.
///
/// # Safety
///
/// `header` must point to a cell that no other thread uses, with `data`,
/// `class` and `inline_capacity` already set.
#[inline]
unsafe fn init_header(header: NonNull<ObjectHeader>, ty: &Type, class: u8) {
    let hdr = header.as_ptr();
    (*hdr).ty = ty.data_ptr().as_ptr();
    (*hdr).roots.store(0, Ordering::Relaxed);
    (*hdr).color = COLOR_WHITE;
    (*hdr).flags = 0;
    (*hdr).class = class;
    (*hdr).state.store(STATE_ALLOCATED, Ordering::Release);
}

impl Drop for HeapInner {
    fn drop(&mut self) {
        for (_, (payload, layout)) in self.out_of_line.drain() {
            unsafe { std::alloc::dealloc(payload.as_ptr(), layout) };
        }
        for (address, layout) in self.large.drain() {
            unsafe { std::alloc::dealloc(address as *mut u8, layout) };
        }
        for block in self.blocks.drain(..) {
            unsafe { std::alloc::dealloc(block.base.as_ptr(), block_layout()) };
        }
        for base in self.empty_blocks.drain(..) {
            unsafe { std::alloc::dealloc(base.as_ptr(), block_layout()) };
        }
    }
}

/// Source of unique heap identifiers. Identifiers are never reused, so a
/// thread cache can never be confused with a cache of a dropped heap.
static NEXT_HEAP_ID: AtomicU64 = AtomicU64::new(1);

/// A garbage-collected heap.
pub(crate) struct Heap {
    id: u64,
    pub(crate) inner: RwLock<HeapInner>,
    /// Pacing asks for a collection (see [`HeapInner::check_trigger`]).
    /// Read without the lock on every allocation by Codira code.
    pub(crate) due: Arc<AtomicBool>,
    /// Incremented whenever the type registry is pruned, which invalidates
    /// the thread caches' sets of known-registered types.
    type_epoch: AtomicU64,
}

// Safety: all raw pointers in `HeapInner` point to memory owned by the heap,
// and all access to it is synchronized by the heap lock or the per-cell
// ownership protocol described in the module documentation.
unsafe impl Send for HeapInner {}
unsafe impl Sync for HeapInner {}

impl Heap {
    pub(crate) fn new(limit: Option<usize>) -> Arc<Self> {
        let due = Arc::new(AtomicBool::new(false));
        Arc::new(Self {
            id: NEXT_HEAP_ID.fetch_add(1, Ordering::Relaxed),
            inner: RwLock::new(HeapInner::new(limit, due.clone())),
            due,
            type_epoch: AtomicU64::new(0),
        })
    }

    /// Allocates an object of type `ty` with a payload of `payload`. The
    /// payload is zeroed.
    #[inline]
    pub(crate) fn alloc(
        self: &Arc<Self>,
        ty: &Type,
        payload: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        match size_class(payload) {
            Some(class) => {
                let fast = THREAD_CACHES.try_with(|caches| {
                    let Ok(mut caches) = caches.try_borrow_mut() else {
                        return None;
                    };
                    let cache = ThreadCaches::get_or_insert(&mut caches, self);
                    Some(cache.alloc(self, ty, class, payload.size()))
                });
                match fast {
                    Ok(Some(result)) => result,
                    // The thread is shutting down or re-entered the allocator:
                    // fall back to the locked path.
                    _ => self.inner.write().alloc_locked(ty, payload),
                }
            }
            None => self.inner.write().alloc_locked(ty, payload),
        }
    }

    /// Invalidates the thread caches' known-type sets. Must be called after
    /// the type registry was pruned.
    pub(crate) fn bump_type_epoch(&self) {
        self.type_epoch.fetch_add(1, Ordering::Release);
    }

    /// Returns the exact number of payload bytes of live objects.
    pub(crate) fn allocated_bytes(&self) -> usize {
        self.inner.read().allocated_bytes()
    }

    /// Returns the bytes currently committed by the heap.
    pub(crate) fn committed(&self) -> usize {
        self.inner.read().committed()
    }

    /// Returns true if pacing asked for a collection.
    #[inline]
    pub(crate) fn collection_due(&self) -> bool {
        self.due.load(Ordering::Relaxed)
    }
}

/// The number of entries in a thread cache's known-type table.
const KNOWN_TYPES: usize = 64;

/// A run of cells a thread may bump-allocate from without locking.
#[derive(Clone, Copy, Default)]
struct LocalRun {
    start: usize,
    cur: usize,
    end: usize,
}

/// Per-thread, per-heap allocation state.
struct ThreadHeapCache {
    heap_id: u64,
    heap: Weak<Heap>,
    /// Payload bytes and objects this thread allocated, ever. Only this
    /// thread writes them.
    counter: Arc<ThreadCounters>,
    type_epoch: u64,
    /// Direct-mapped set of type data addresses known to be registered.
    known_types: [usize; KNOWN_TYPES],
    runs: [LocalRun; NUM_CLASSES],
}

impl ThreadHeapCache {
    fn new(heap: &Arc<Heap>) -> Self {
        let counter = Arc::new(ThreadCounters::default());
        heap.inner.write().thread_counters.push(counter.clone());
        Self {
            heap_id: heap.id,
            heap: Arc::downgrade(heap),
            counter,
            type_epoch: heap.type_epoch.load(Ordering::Acquire),
            known_types: [0; KNOWN_TYPES],
            runs: [LocalRun::default(); NUM_CLASSES],
        }
    }

    #[inline]
    fn alloc(
        &mut self,
        heap: &Heap,
        ty: &Type,
        class: u8,
        payload_size: usize,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        // Make sure the heap's registry keeps the type alive.
        let epoch = heap.type_epoch.load(Ordering::Acquire);
        if epoch != self.type_epoch {
            self.known_types = [0; KNOWN_TYPES];
            self.type_epoch = epoch;
        }
        let type_address = ty.data_ptr().as_ptr() as usize;
        let slot = (type_address >> 4) % KNOWN_TYPES;
        if self.known_types[slot] != type_address {
            heap.inner.write().register_type(ty);
            self.known_types[slot] = type_address;
        }

        let cell_size = class_size(class);
        let run = &mut self.runs[class as usize];
        if run.cur >= run.end {
            let mut inner = heap.inner.write();
            if run.start != 0 {
                inner.end_lease(run.start, run.cur, run.end);
            }
            *run = LocalRun::default();
            let new_run = inner.lease_run(class)?;
            *run = LocalRun {
                start: new_run.start,
                cur: new_run.start,
                end: new_run.end,
            };
        }
        let cell = run.cur;
        run.cur += cell_size;

        let header = unsafe { NonNull::new_unchecked(cell as *mut ObjectHeader) };
        unsafe {
            let payload = (cell + HEADER_SIZE) as *mut u8;
            std::ptr::write_bytes(payload, 0, payload_size);
            let hdr = header.as_ptr();
            (*hdr).data = payload;
            (*hdr).inline_capacity = cell_size - HEADER_SIZE;
            init_header(header, ty, class);
        }

        // Single-writer counters: a load and a store each, no atomic
        // read-modify-write.
        let counter = &*self.counter;
        counter.bytes.store(
            counter.bytes.load(Ordering::Relaxed) + payload_size,
            Ordering::Relaxed,
        );
        counter.objects.store(
            counter.objects.load(Ordering::Relaxed) + 1,
            Ordering::Relaxed,
        );
        Ok(header)
    }
}

impl Drop for ThreadHeapCache {
    fn drop(&mut self) {
        let Some(heap) = self.heap.upgrade() else {
            // The heap and all its memory are gone.
            return;
        };
        let mut inner = heap.inner.write();
        for run in &self.runs {
            if run.start != 0 {
                inner.end_lease(run.start, run.cur, run.end);
            }
        }
        let bytes = self.counter.bytes.load(Ordering::Relaxed);
        inner.global_bytes += bytes as isize;
        inner.retired_bytes_total += bytes as u64;
        inner.retired_objects_total += self.counter.objects.load(Ordering::Relaxed) as u64;
        inner
            .thread_counters
            .retain(|c| !Arc::ptr_eq(c, &self.counter));
    }
}

/// The thread caches of the current thread, most recently used first.
#[derive(Default)]
struct ThreadCaches {
    entries: Vec<ThreadHeapCache>,
}

impl ThreadCaches {
    #[inline]
    fn get_or_insert<'a>(
        caches: &'a mut ThreadCaches,
        heap: &Arc<Heap>,
    ) -> &'a mut ThreadHeapCache {
        if caches.entries.first().map(|c| c.heap_id) != Some(heap.id) {
            Self::promote(caches, heap);
        }
        &mut caches.entries[0]
    }

    #[cold]
    fn promote(caches: &mut ThreadCaches, heap: &Arc<Heap>) {
        if let Some(index) = caches.entries.iter().position(|c| c.heap_id == heap.id) {
            let entry = caches.entries.remove(index);
            caches.entries.insert(0, entry);
        } else {
            // Forget caches of heaps that no longer exist.
            caches.entries.retain(|c| c.heap.strong_count() > 0);
            caches.entries.insert(0, ThreadHeapCache::new(heap));
        }
    }
}

thread_local! {
    static THREAD_CACHES: RefCell<ThreadCaches> = RefCell::new(ThreadCaches::default());
}
