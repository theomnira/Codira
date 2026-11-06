//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Mark-sweep garbage collector over the segregated size-class heap
//!   (`gc::heap`): host-rooted tracing, sweeping, and hot-reload memory
//!   migration.
use std::{
    alloc::Layout,
    collections::HashMap,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    cast,
    gc::{
        array::ArrayHeader,
        heap::{
            array_payload_layout, handle_of, header_of, AllocError, Heap, HeapInner, ObjectHeader,
            PayloadMigration, SweepOutcome, COLOR_BLACK, COLOR_WHITE,
        },
        policy::GcPolicy,
        safepoint,
        stats::{CollectionCounts, CollectionReport, GcStats, PauseHistogram, Trigger},
        Array as GcArray, Event, GcPtr, GcRuntime, Observer, Stats, TypeTrace,
    },
    mapping::{self, resolve_struct_to_struct_edit, Action, FieldMapping, Mapping, MemoryMapper},
    r#type::{Type, TypeData},
};

/// Returns the offset of the first element of an array whose elements are of
/// type `element`, relative to the start of the array payload.
pub(crate) fn array_data_offset(element: &TypeData) -> usize {
    Layout::new::<ArrayHeader>()
        .extend(element.reference_layout())
        .expect("error creating combined layout of header and element")
        .1
}

/// Calls `f` for every non-null GC reference stored in the payload of
/// `header`.
///
/// # Safety
///
/// `header` must be an allocated object whose payload matches its type.
#[inline]
unsafe fn for_each_reference(header: &ObjectHeader, mut f: impl FnMut(NonNull<ObjectHeader>)) {
    let ty = header.type_data();
    let payload = header.data;
    let mut visit = |slot: *const u8| {
        if let Some(child) = NonNull::new(*slot.cast::<*mut ObjectHeader>()) {
            f(child);
        }
    };

    if let Some(element) = ty.array_element() {
        let length = (*payload.cast::<ArrayHeader>()).length;
        if length == 0 {
            return;
        }
        let elements = payload.add(array_data_offset(element));
        let stride = element.reference_layout().pad_to_align().size();
        if element.is_reference() {
            for index in 0..length {
                visit(elements.add(index * stride));
            }
        } else {
            let offsets = element.ref_offsets();
            if offsets.is_empty() {
                return;
            }
            for index in 0..length {
                let element_ptr = elements.add(index * stride);
                for &offset in offsets {
                    visit(element_ptr.add(offset as usize));
                }
            }
        }
    } else {
        for &offset in ty.ref_offsets() {
            visit(payload.add(offset as usize));
        }
    }
}

/// An iterator over all GC objects referenced by another object.
pub struct Trace {
    references: std::vec::IntoIter<GcPtr>,
}

impl Iterator for Trace {
    type Item = GcPtr;

    fn next(&mut self) -> Option<Self::Item> {
        self.references.next()
    }
}

impl TypeTrace for Type {
    type Trace = Trace;

    fn trace(&self, obj: GcPtr) -> Self::Trace {
        let header = header_of(obj);
        let mut references = Vec::new();
        // Safety: the caller provides a handle to an allocated object.
        unsafe {
            for_each_reference(header.as_ref(), |child| references.push(handle_of(child)));
        }
        Trace {
            references: references.into_iter(),
        }
    }
}

/// Implements a mark-sweep garbage collector.
///
/// Objects live in a segregated size-class heap with inline headers (see the
/// `gc::heap` module). Roots are the objects the host rooted through
/// [`GcRuntime::root`], plus -- because generated code has no stack maps --
/// every word on the stack of every thread that runs Codira code, scanned
/// conservatively. A collection stops the world first: it waits until every
/// such thread has parked at a safepoint (an allocation) or in a blocking
/// runtime call, scans their stacks, collects, and resumes them (see
/// [`safepoint`]).
///
/// A collection starts in one of four ways (see [`Trigger`]):
///
/// - **pacing**: the heap grew by the [`GcPolicy`]'s percentage over what
///   the previous collection left live; the next allocation by a thread
///   running Codira code collects;
/// - **the heap limit**: an allocation would exceed it; it collects and
///   retries once, and fails with [`AllocError::LimitExceeded`] if the
///   memory is still not there;
/// - [`MarkSweep::request_collection`]: returns at once; the collection runs
///   on a thread of the collector's own;
/// - [`MarkSweep::collect_blocking`]: returns when the collection is done.
///
/// The last two are requests from outside. They wait until Codira code has
/// run for the policy's mutator window since the previous collection, and a
/// request is folded into any collection that starts after it was made, so
/// no loop of requests can starve the threads running Codira code.
pub struct MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    pub(crate) heap: Arc<Heap>,
    core: Arc<Core<O>>,
    /// Requests for [`MarkSweep::request_collection`]'s thread.
    requests: Arc<Requests>,
    /// That thread, once started.
    request_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Says where the heap limit came from, for the diagnostic of a failed
    /// allocation (see [`MarkSweep::set_heap_limit_note`]).
    limit_note: Mutex<Option<String>>,
}

/// What a collection needs, shared with [`MarkSweep::request_collection`]'s
/// thread.
struct Core<O> {
    heap: Arc<Heap>,
    observer: O,
    policy: Mutex<GcPolicy>,
    accounting: Mutex<Accounting>,
    /// Collections started, ever. Written with the world stopped.
    started: AtomicU64,
}

/// The collector's statistics and the state its fairness rule needs.
#[derive(Default)]
struct Accounting {
    counts: CollectionCounts,
    bytes_reclaimed: u64,
    objects_reclaimed: u64,
    time_to_safepoint: Duration,
    pauses: PauseHistogram,
    /// When the previous collection ended, and how long it paused.
    last_end: Option<Instant>,
    last_pause: Duration,
    last_report: CollectionReport,
}

/// Pending [`MarkSweep::request_collection`] calls.
#[derive(Default)]
struct Requests {
    state: Mutex<RequestState>,
    wake: Condvar,
}

#[derive(Default)]
struct RequestState {
    /// A request is waiting for a collection to start.
    pending: bool,
    /// [`Core::started`] when the first pending request was made: a
    /// collection that starts after that satisfies it.
    started_at_request: u64,
    /// The collector is being dropped.
    shutdown: bool,
}

impl<O> Default for MarkSweep<O>
where
    O: Observer<Event = Event> + Default,
{
    fn default() -> Self {
        Self::with_observer(O::default())
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    /// Creates a `MarkSweep` memory collector with the specified `Observer`
    /// and the default [`GcPolicy`] (pacing on, no heap limit).
    pub fn with_observer(observer: O) -> Self {
        let heap = Heap::new(None);
        let policy = GcPolicy::default();
        heap.inner
            .write()
            .set_pacing(policy.growth_percent, policy.min_heap);
        Self {
            heap: heap.clone(),
            core: Arc::new(Core {
                heap,
                observer,
                policy: Mutex::new(policy),
                accounting: Mutex::new(Accounting::default()),
                started: AtomicU64::new(0),
            }),
            requests: Arc::new(Requests::default()),
            request_thread: Mutex::new(None),
            limit_note: Mutex::new(None),
        }
    }

    /// Returns the observer
    pub fn observer(&self) -> &O {
        &self.core.observer
    }

    /// Returns the bytes the heap currently holds from the system:
    /// size-class blocks (including cached empty blocks), individually
    /// allocated large objects and out-of-line payloads. This is larger than
    /// [`Stats::allocated_memory`], which only counts live payload bytes.
    pub fn committed_bytes(&self) -> usize {
        self.heap.committed()
    }

    /// Limits the bytes the heap may hold from the system (size-class blocks,
    /// large objects and out-of-line payloads; see
    /// [`MarkSweep::committed_bytes`]). `None` removes the limit. Lowering
    /// the limit below the current size releases nothing; the heap simply
    /// stops growing.
    ///
    /// This caps the garbage-collected heap only: raw allocations a program
    /// makes outside the GC (arenas, `codira_alloc`, pages) are not counted.
    pub fn set_heap_limit(&self, limit: Option<usize>) {
        self.heap.inner.write().set_limit(limit);
    }

    /// Returns the heap limit, if any.
    pub fn heap_limit(&self) -> Option<usize> {
        self.heap.inner.read().limit()
    }

    /// Records where the heap limit came from (a flag, a setting, a
    /// default), in words a user can act on. The runtime prints it with the
    /// diagnostic of an allocation that fails at the limit.
    pub fn set_heap_limit_note(&self, note: Option<String>) {
        *self.limit_note.lock() = note;
    }

    /// Returns what [`MarkSweep::set_heap_limit_note`] recorded.
    pub fn heap_limit_note(&self) -> Option<String> {
        self.limit_note.lock().clone()
    }

    /// Sets when the collector runs besides at the heap limit (pacing) and
    /// how requests from outside share time with Codira code (see
    /// [`GcPolicy`]).
    pub fn set_policy(&self, policy: GcPolicy) {
        self.heap
            .inner
            .write()
            .set_pacing(policy.growth_percent, policy.min_heap);
        *self.core.policy.lock() = policy;
    }

    /// Returns the current [`GcPolicy`].
    pub fn policy(&self) -> GcPolicy {
        self.core.policy.lock().clone()
    }

    /// Returns how many collections were started because an allocation hit
    /// the heap limit.
    pub fn emergency_collections(&self) -> usize {
        self.core.accounting.lock().counts.at_limit as usize
    }

    /// Returns the heap's statistics: its size, allocation totals,
    /// collections by cause, what they reclaimed and how long they paused.
    ///
    /// Counting costs a load and a store of two thread-local counters per
    /// allocation, and a few clock readings per collection; reading the
    /// statistics takes the heap lock briefly. Allocation totals of threads
    /// that are allocating at the moment may be a few objects behind.
    pub fn heap_stats(&self) -> GcStats {
        let counters = self.heap.inner.read().counters();
        let accounting = self.core.accounting.lock();
        let pauses = accounting.pauses.summary();
        GcStats {
            heap_bytes: counters.committed,
            peak_heap_bytes: counters.peak_committed,
            heap_limit: counters.limit,
            live_bytes: counters.live,
            live_bytes_after_last_collection: counters.live_after_sweep,
            next_paced_collection_at: counters.trigger,
            bytes_allocated: counters.bytes_allocated,
            objects_allocated: counters.objects_allocated,
            collections: accounting.counts,
            bytes_reclaimed: accounting.bytes_reclaimed,
            objects_reclaimed: accounting.objects_reclaimed,
            collection_time: pauses.total,
            time_to_safepoint: accounting.time_to_safepoint,
            pauses,
        }
    }

    /// Allocates an object of type `ty`, or returns why the heap could not
    /// provide the memory.
    ///
    /// Every allocation is a safepoint: a thread running Codira code parks
    /// here while another thread collects. An allocation also collects first
    /// when pacing asked for a collection (see [`GcPolicy`]).
    ///
    /// When the allocation would exceed the heap limit, the collector runs
    /// first (stopping the world, so every thread running Codira code is
    /// parked and its stack scanned conservatively, this thread's included)
    /// and the allocation is retried once. If the collection cannot run
    /// safely -- the current thread is exiting, is itself collecting or
    /// migrating, or some thread's stack cannot be scanned on this platform
    /// -- the allocation fails with [`AllocError::LimitExceeded`] without
    /// collecting. After a collection, the error's `live` is what survived
    /// it.
    pub fn try_alloc(&self, ty: &Type) -> Result<GcPtr, AllocError> {
        assert!(ty.is_concrete());

        let header = self.alloc_payload(ty, ty.value_layout())?;
        let handle = handle_of(header);
        self.core.observer.event(Event::Allocation(handle));
        Ok(handle)
    }

    /// Allocates an array of type `ty` with `n` elements, or returns why the
    /// heap could not provide the memory. See [`MarkSweep::try_alloc`].
    pub fn try_alloc_array(&self, ty: &Type, n: usize) -> Result<ArrayHandle, AllocError> {
        let element = unsafe { ty.data_ptr().as_ref() }
            .array_element()
            .expect("array type doesnt have an element type");
        let layout = array_payload_layout(element, n).ok_or(AllocError::TooLarge)?;
        let header = self.alloc_payload(ty, layout)?;
        unsafe { init_array_header(header.as_ref().data, n) };
        self.core
            .observer
            .event(Event::Allocation(handle_of(header)));
        Ok(ArrayHandle { obj: header })
    }

    /// Makes room in the array `handle` for at least `min_capacity`
    /// elements, or returns why the heap could not provide the memory.
    ///
    /// The capacity at least doubles (and is at least 4), so pushing `n`
    /// elements one at a time copies O(n) elements in total. The array keeps
    /// its handle; its elements move to a new buffer (see
    /// `HeapInner::grow_array`). Like an allocation this is a safepoint, a
    /// paced collection may run first, and at the heap limit it collects
    /// once and retries, the array being reachable from the caller's stack.
    ///
    /// # Safety
    ///
    /// `handle` must be a live array of this heap that no other thread
    /// accesses during the call.
    pub unsafe fn try_grow_array(
        &self,
        handle: GcPtr,
        min_capacity: usize,
    ) -> Result<(), AllocError> {
        let header = header_of(handle);
        let capacity = (*header.as_ref().data.cast::<ArrayHeader>()).capacity;
        if min_capacity <= capacity {
            return Ok(());
        }
        let new_capacity = min_capacity.max(capacity.saturating_mul(2)).max(4);
        let grow = || unsafe { self.heap.inner.write().grow_array(header, new_capacity) };
        let grow_or_collect = || match grow() {
            Err(err @ AllocError::LimitExceeded { .. }) if safepoint::is_running() => self
                .core
                .collect_stopped_then(Trigger::HeapLimit, |_| false, grow)
                .map_err(|CollectRefused| err)
                .and_then(|(_, result)| result)
                .map_err(|err| self.core.refresh_live(err)),
            result => result,
        };
        if safepoint::is_running() {
            safepoint::poll();
            if self.heap.collection_due() {
                self.core.collect_for_pacing();
            }
            grow_or_collect()
        } else if safepoint::holds_stop() {
            grow()
        } else {
            safepoint::with_allocation_section(grow_or_collect)
        }
    }

    /// Allocates an object with a payload of `layout`, collecting first if
    /// pacing asked for it, and once more if the heap limit is hit, when
    /// that is safe.
    #[inline]
    fn alloc_payload(
        &self,
        ty: &Type,
        layout: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        if safepoint::is_running() {
            safepoint::poll();
            if self.heap.collection_due() {
                self.core.collect_for_pacing();
            }
            self.alloc_or_collect(ty, layout)
        } else if safepoint::holds_stop() {
            // Every other thread is parked, and this one must not start a
            // collection in the middle of its own stop.
            self.heap.alloc(ty, layout)
        } else {
            // Not running Codira code: allocate in a brief running section,
            // so no collection can sweep the cell while it is initialized.
            // Collecting in it scans this thread's stack like a mutator's.
            safepoint::with_allocation_section(|| {
                if safepoint::is_running() && self.heap.collection_due() {
                    self.core.collect_for_pacing();
                }
                self.alloc_or_collect(ty, layout)
            })
        }
    }

    fn alloc_or_collect(
        &self,
        ty: &Type,
        layout: Layout,
    ) -> Result<NonNull<ObjectHeader>, AllocError> {
        match self.heap.alloc(ty, layout) {
            Err(err @ AllocError::LimitExceeded { .. }) => {
                // Collecting needs this thread to be able to park; a thread
                // that is exiting cannot.
                if !safepoint::is_running() {
                    return Err(err);
                }
                // Retry while the world is still stopped: otherwise the
                // other threads, resumed first, could use up what the
                // collection freed before this thread gets any of it.
                //
                // The new object is rooted until this thread runs again: on
                // the way out of the stop it is held in frames below the
                // part of the stack a collection would scan, and another
                // thread may collect before this one resumes.
                let (_, result) = self
                    .core
                    .collect_stopped_then(
                        Trigger::HeapLimit,
                        |_| false,
                        || {
                            let result = self.heap.alloc(ty, layout);
                            if let Ok(header) = result {
                                unsafe { header.as_ref() }
                                    .roots
                                    .fetch_add(1, Ordering::AcqRel);
                            }
                            result
                        },
                    )
                    .map_err(|CollectRefused| err)?;
                match result {
                    Ok(header) => {
                        unsafe { header.as_ref() }
                            .roots
                            .fetch_sub(1, Ordering::AcqRel);
                        Ok(header)
                    }
                    Err(err) => Err(self.core.refresh_live(err)),
                }
            }
            result => result,
        }
    }

    /// Collects all memory that is no longer referenced by rooted objects
    /// or from the stack of a thread running Codira code, and returns what
    /// was reclaimed once the collection is done.
    ///
    /// Safe to call from any thread at any time: the collection stops the
    /// world first. Before that it waits until Codira code has run for the
    /// [`GcPolicy`]'s mutator window since the previous collection (if some
    /// other thread is inside Codira code), so calling this in a loop cannot
    /// starve the threads running Codira code; the wait happens parked, so
    /// it never holds up another thread's collection. A call is folded into
    /// a collection that another thread starts after the call began
    /// ([`CollectionReport::coalesced`]). The stop itself waits for every
    /// thread running Codira code to reach a safepoint, so it can take as
    /// long as the longest stretch of Codira code that neither allocates nor
    /// blocks.
    ///
    /// Fails with [`CollectRefused`] when some thread inside Codira code is
    /// parked on a platform whose stacks cannot be scanned, so collecting
    /// could free objects only it refers to.
    pub fn collect_blocking(&self) -> Result<CollectionReport, CollectRefused> {
        let started_at_call = self.core.started.load(Ordering::SeqCst);
        self.core.wait_for_window(started_at_call, |wait| {
            safepoint::blocking(|| std::thread::sleep(wait));
            true
        });
        self.core
            .collect_external(Trigger::Explicit, started_at_call)
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event> + 'static,
{
    /// Asks for a collection without waiting for it. Returns `true` if this
    /// call queued a request, `false` if one was already pending (the
    /// requests are coalesced into one collection).
    ///
    /// The collection runs on a thread of the collector's own (started by
    /// the first request), as soon as Codira code has run for the
    /// [`GcPolicy`]'s mutator window since the previous collection; a
    /// collection of any kind that starts after the request satisfies it.
    /// [`GcStats::collections`] counts the requests served.
    pub fn request_collection(&self) -> bool {
        let queued = {
            let mut state = self.requests.state.lock();
            if state.shutdown {
                return false;
            }
            let queued = !state.pending;
            if queued {
                state.pending = true;
                state.started_at_request = self.core.started.load(Ordering::SeqCst);
            }
            queued
        };
        self.requests.wake.notify_all();
        if queued {
            self.ensure_request_thread();
        }
        queued
    }

    fn ensure_request_thread(&self) {
        let mut thread = self.request_thread.lock();
        if thread.is_some() {
            return;
        }
        let core = Arc::downgrade(&self.core);
        let requests = self.requests.clone();
        match std::thread::Builder::new()
            .name("codira-gc-requests".to_string())
            .spawn(move || serve_requests(&core, &requests))
        {
            Ok(handle) => *thread = Some(handle),
            Err(_spawn_error) => {
                // No thread to serve it: let the next allocation by Codira
                // code collect instead, as pacing would.
                self.heap.due.store(true, Ordering::Relaxed);
                self.requests.state.lock().pending = false;
            }
        }
    }
}

/// The loop of [`MarkSweep::request_collection`]'s thread.
fn serve_requests<O: Observer<Event = Event>>(core: &Weak<Core<O>>, requests: &Requests) {
    loop {
        let started_at_request = {
            let mut state = requests.state.lock();
            while !state.pending && !state.shutdown {
                requests.wake.wait(&mut state);
            }
            if state.shutdown {
                return;
            }
            state.started_at_request
        };
        let Some(core) = core.upgrade() else {
            return;
        };
        let waited = core.wait_for_window(started_at_request, |wait| {
            let mut state = requests.state.lock();
            if !state.shutdown {
                requests.wake.wait_for(&mut state, wait);
            }
            !state.shutdown
        });
        if !waited {
            return;
        }
        // Requests made from here on wait for the next collection.
        requests.state.lock().pending = false;
        // A refusal (an unscannable thread) leaves nothing to do but wait
        // for the next request.
        let _refused = core.collect_external(Trigger::Requested, started_at_request);
    }
}

impl<O> Drop for MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    fn drop(&mut self) {
        self.requests.state.lock().shutdown = true;
        self.requests.wake.notify_all();
        if let Some(thread) = self.request_thread.get_mut().take() {
            // The thread may be stopping the world; park while waiting for
            // it, in case this thread runs Codira code.
            safepoint::blocking(|| {
                let _ = thread.join();
            });
        }
    }
}

impl<O> Core<O>
where
    O: Observer<Event = Event>,
{
    /// Runs the collection pacing asked for, unless another thread ran one
    /// since.
    #[cold]
    #[inline(never)]
    fn collect_for_pacing(&self) {
        let outcome =
            self.collect_stopped_then(Trigger::Pacing, |core| !core.heap.collection_due(), || ());
        if outcome.is_err() {
            // Collecting is not possible right now; do not retry on every
            // allocation (the next slow path asks again).
            self.heap.due.store(false, Ordering::Relaxed);
        }
    }

    /// Waits until Codira code has had its mutator window since the
    /// previous collection, no other thread is inside Codira code, or a
    /// collection started after [`Core::started`] was `started_at_request`
    /// (which satisfies the request). `sleep` waits (parked) for up to the
    /// given time and returns false to give up; then this returns false too.
    fn wait_for_window(
        &self,
        started_at_request: u64,
        mut sleep: impl FnMut(Duration) -> bool,
    ) -> bool {
        /// Waits are cut into slices of at most this, to notice a collection
        /// that satisfies the request.
        const SLICE: Duration = Duration::from_millis(1);
        loop {
            if self.started.load(Ordering::SeqCst) > started_at_request {
                return true;
            }
            let remaining = {
                let accounting = self.accounting.lock();
                let Some(last_end) = accounting.last_end else {
                    return true;
                };
                let window = self.policy.lock().mutator_window(accounting.last_pause);
                (last_end + window).checked_duration_since(Instant::now())
            };
            match remaining {
                Some(wait) if !wait.is_zero() && safepoint::other_threads_in_codira() => {
                    if !sleep(wait.min(SLICE)) {
                        return false;
                    }
                }
                _ => return true,
            }
        }
    }

    /// Collects for a request from outside made when [`Core::started`] was
    /// `started_at_request`, unless a collection started since.
    fn collect_external(
        &self,
        trigger: Trigger,
        started_at_request: u64,
    ) -> Result<CollectionReport, CollectRefused> {
        let (report, ()) = self.collect_stopped_then(
            trigger,
            |core| core.started.load(Ordering::SeqCst) > started_at_request,
            || (),
        )?;
        Ok(report.unwrap_or_else(|| CollectionReport {
            coalesced: true,
            ..self.accounting.lock().last_report
        }))
    }

    /// Fills in the live bytes of a [`AllocError::LimitExceeded`] after the
    /// collection that preceded it.
    fn refresh_live(&self, err: AllocError) -> AllocError {
        match err {
            AllocError::LimitExceeded {
                limit,
                committed,
                requested,
                ..
            } => AllocError::LimitExceeded {
                limit,
                committed,
                requested,
                live: self.heap.allocated_bytes(),
            },
            other => other,
        }
    }

    /// Stops the world and, unless `skip` says a collection is no longer
    /// needed, collects; then runs `then` before resuming the other
    /// threads. Returns what the collection did (`None` if skipped) and
    /// what `then` returned.
    fn collect_stopped_then<R>(
        &self,
        trigger: Trigger,
        skip: impl FnOnce(&Self) -> bool,
        then: impl FnOnce() -> R,
    ) -> Result<(Option<CollectionReport>, R), CollectRefused> {
        safepoint::stop_the_world(|world| {
            if skip(self) {
                return Ok((None, then()));
            }
            if world.has_unscannable_threads() {
                return Err(CollectRefused);
            }
            self.started.fetch_add(1, Ordering::SeqCst);
            let mut words = world.stack_words();
            let swept = if words.is_empty() {
                self.collect_impl(None)
            } else {
                self.collect_impl(Some(&mut words))
            };
            let value = then();
            let end = Instant::now();
            let report = CollectionReport {
                reclaimed_objects: swept.objects,
                reclaimed_bytes: swept.bytes,
                pause: end.saturating_duration_since(world.requested_at),
                coalesced: false,
            };
            let mut accounting = self.accounting.lock();
            accounting.counts.count(trigger);
            accounting.bytes_reclaimed += swept.bytes as u64;
            accounting.objects_reclaimed += swept.objects as u64;
            accounting.time_to_safepoint += world
                .stopped_at
                .saturating_duration_since(world.requested_at);
            accounting.pauses.record(report.pause);
            accounting.last_end = Some(end);
            accounting.last_pause = report.pause;
            accounting.last_report = report;
            Ok((Some(report), value))
        })
    }

    fn collect_impl(&self, stack_words: Option<&mut Vec<usize>>) -> SweepOutcome {
        self.observer.event(Event::Start);

        let mut inner = self.heap.inner.write();

        // In debug builds, validate every traced reference before following it.
        #[cfg(debug_assertions)]
        let known: FxHashSet<usize> = {
            let mut known = FxHashSet::default();
            inner.for_each_object(|o| {
                known.insert(o.as_ptr() as usize);
            });
            known
        };

        // Mark all roots
        let mut stack: Vec<NonNull<ObjectHeader>> = Vec::new();
        inner.for_each_object(|object| {
            let header = object.as_ptr();
            unsafe {
                if (*header).roots.load(std::sync::atomic::Ordering::Acquire) > 0
                    && (*header).color == COLOR_WHITE
                {
                    (*header).color = COLOR_BLACK;
                    stack.push(object);
                }
            }
        });

        // Objects the scanned stack may refer to are roots too
        if let Some(words) = stack_words {
            for object in inner.objects_referenced_by(words) {
                let header = object.as_ptr();
                unsafe {
                    if (*header).color == COLOR_WHITE {
                        (*header).color = COLOR_BLACK;
                        stack.push(object);
                    }
                }
            }
        }

        // Trace everything reachable from the roots
        while let Some(object) = stack.pop() {
            unsafe {
                for_each_reference(object.as_ref(), |child| {
                    #[cfg(debug_assertions)]
                    assert!(
                        known.contains(&(child.as_ptr() as usize)),
                        "found invalid reference"
                    );
                    let child_header = child.as_ptr();
                    if (*child_header).color == COLOR_WHITE {
                        (*child_header).color = COLOR_BLACK;
                        stack.push(child);
                    }
                });
            }
        }

        let swept = inner.sweep(|handle| self.observer.event(Event::Deallocation(handle)));
        drop(inner);

        self.observer.event(Event::End);

        swept
    }
}

/// [`MarkSweep::collect_blocking`] refused to collect: a thread inside
/// Codira code cannot have its stack scanned on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a thread running Codira code cannot have its stack scanned on this platform")]
pub struct CollectRefused;

/// Helper object to work with [`GcPtr`] that represents an array.
///
/// Arrays are stored in memory with a header which holds the length and
/// capacity. The memory layout of an array looks like this in memory:
///
/// ```text
/// object.data ───►┌──────────────┐
///                 │ ArrayHeader  │
///                 └─┬────────────┘
///                   │ padding to align elements
///                 ┌─┴────────────┐
///                 │ element #1   │
///                 └──────────────┘
///                  :
///                 ┌──────────────┐
///                 │ element #n   │
///                 └──────────────┘
/// ```
pub struct ArrayHandle {
    /// Pointer to the object header.
    obj: NonNull<ObjectHeader>,
}

impl ArrayHandle {
    /// Returns a reference to the header
    pub fn header(&self) -> &ArrayHeader {
        // Safety: the handle refers to an allocated array object.
        unsafe { &*self.obj.as_ref().data.cast::<ArrayHeader>() }
    }

    /// Sets the length of the array.
    ///
    /// # Safety
    ///
    /// This function is unsafe because the array elements might be left
    /// uninitialized.
    pub unsafe fn set_length(&mut self, length: usize) {
        let header = &mut *self.obj.as_ref().data.cast::<ArrayHeader>();
        debug_assert!(header.capacity >= length);
        header.length = length;
    }

    /// Returns the layout of an element stored in the array.
    ///
    /// Note that this may be different from the layout of the
    /// [`Self::element_type`]. If the element type is a garbage collected
    /// type, the array stores references instead of raw elements.
    pub fn element_layout(&self) -> Layout {
        self.element_type().reference_layout()
    }

    /// Returns the stride in bytes between elements.
    ///
    /// The stride is determined by the size of [`Self::element_layout`] padded
    /// to alignment of layout.
    pub fn element_stride(&self) -> usize {
        self.element_layout().pad_to_align().size()
    }

    /// Returns a pointer to the data.
    pub fn data(&self) -> NonNull<u8> {
        unsafe {
            let header = self.obj.as_ref();
            let element = header
                .type_data()
                .array_element()
                .expect("unable to determine element_type, type is not an array");
            NonNull::new_unchecked(header.data.add(array_data_offset(element)))
        }
    }
}

impl GcArray for ArrayHandle {
    type Iterator = ArrayHandleIter;

    fn as_raw(&self) -> GcPtr {
        handle_of(self.obj)
    }

    fn element_type(&self) -> Type {
        let array_ty = unsafe { self.obj.as_ref().type_view() };
        array_ty
            .as_array()
            .expect("unable to determine element_type, type is not an array")
            .element_type()
    }

    fn length(&self) -> usize {
        self.header().length
    }

    fn capacity(&self) -> usize {
        self.header().capacity
    }

    fn elements(&self) -> Self::Iterator {
        ArrayHandleIter {
            remaining: self.length(),
            next: self.data(),
            stride: self.element_stride(),
        }
    }
}

/// An iterator implementation.
///
/// TODO: Note that this iterator is highly non-thread safe. Any operation that
/// modifies the original array could cause undefined behavior.
pub struct ArrayHandleIter {
    /// Pointer to the next element
    next: NonNull<u8>,

    /// The number of remaning elements in the iterator.
    remaining: usize,

    /// The number of bytes to skip to get to the next element
    stride: usize,
}

impl Iterator for ArrayHandleIter {
    type Item = NonNull<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining > 0 {
            let element = self.next;
            self.remaining -= 1;
            self.next = unsafe { NonNull::new_unchecked(self.next.as_ptr().add(self.stride)) };
            Some(element)
        } else {
            None
        }
    }
}

/// Returns the payload layout of an array of type `array_ty` with `length`
/// elements.
fn array_layout(array_ty: &Type, length: usize) -> Layout {
    let element = unsafe { array_ty.data_ptr().as_ref() }
        .array_element()
        .expect("array type doesnt have an element type");
    array_payload_layout(element, length).expect("unable to create memory layout for array")
}

/// Writes the length and capacity of a freshly allocated array payload.
unsafe fn init_array_header(payload: *mut u8, length: usize) {
    let header = &mut *payload.cast::<ArrayHeader>();
    header.length = length;
    header.capacity = length;
}

impl<O> GcRuntime for MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    type Array = ArrayHandle;

    fn alloc(&self, ty: &Type) -> GcPtr {
        self.try_alloc(ty)
            .unwrap_or_else(|err| panic!("GC allocation of `{}` failed: {err}", ty.name()))
    }

    fn alloc_array(&self, ty: &Type, n: usize) -> Self::Array {
        self.try_alloc_array(ty, n)
            .unwrap_or_else(|err| panic!("GC allocation of `{}` failed: {err}", ty.name()))
    }

    fn ptr_type(&self, handle: GcPtr) -> Type {
        let _lock = self.heap.inner.read();
        unsafe { header_of(handle).as_ref().type_view().clone() }
    }

    fn array(&self, handle: GcPtr) -> Option<Self::Array> {
        let _lock = self.heap.inner.read();
        let obj = header_of(handle);
        if unsafe { obj.as_ref().type_data() }.is_array() {
            Some(ArrayHandle { obj })
        } else {
            None
        }
    }

    fn root(&self, handle: GcPtr) {
        unsafe { header_of(handle).as_ref() }
            .roots
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn unroot(&self, handle: GcPtr) {
        unsafe { header_of(handle).as_ref() }
            .roots
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn stats(&self) -> Stats {
        Stats {
            allocated_memory: self.heap.allocated_bytes(),
        }
    }
}

/// State shared by the migration helpers of [`MarkSweep::map_memory`].
struct MigrationContext<'a> {
    inner: &'a mut HeapInner,
    conversions: &'a HashMap<Type, mapping::StructMapping>,
    /// Payload migrations in flight, keyed by header address, with the type
    /// the object gets once the migration completes.
    migrations: FxHashMap<usize, (PayloadMigration, Type)>,
    /// Objects allocated while migrating, reported to the observer afterwards.
    new_objects: Vec<GcPtr>,
}

impl MigrationContext<'_> {
    /// Allocates a new object during migration.
    fn alloc(&mut self, ty: &Type, layout: Layout) -> NonNull<ObjectHeader> {
        let header = self
            .inner
            .alloc_locked(ty, layout)
            .unwrap_or_else(|err: AllocError| panic!("allocation during migration failed: {err}"));
        self.new_objects.push(handle_of(header));
        header
    }

    /// Returns the payload of `header` as it was before migration started.
    fn old_payload(&self, header: NonNull<ObjectHeader>) -> NonNull<u8> {
        match self.migrations.get(&(header.as_ptr() as usize)) {
            Some((migration, _)) => migration.src,
            None => unsafe { NonNull::new_unchecked(header.as_ref().data) },
        }
    }

    /// Starts migrating `header` to `new_ty` with a payload of `layout`.
    /// Returns `(old payload, new payload)`.
    fn begin(
        &mut self,
        header: NonNull<ObjectHeader>,
        layout: Layout,
        new_ty: &Type,
    ) -> (NonNull<u8>, NonNull<u8>) {
        let migration = unsafe { self.inner.begin_payload_migration(header, layout) }
            .unwrap_or_else(|err| panic!("migrating an object failed: {err}"));
        let pair = (migration.src, migration.dest);
        self.migrations
            .insert(header.as_ptr() as usize, (migration, new_ty.clone()));
        pair
    }

    /// Migrates an array object to `new_ty`, mapping every element with
    /// `element_action`. Arrays that are already being migrated are left
    /// alone.
    fn migrate_array(
        &mut self,
        header: NonNull<ObjectHeader>,
        element_action: &Action,
        new_ty: &Type,
    ) {
        if self.migrations.contains_key(&(header.as_ptr() as usize)) {
            return;
        }
        let old_ty = unsafe { header.as_ref().type_view().clone() };
        let old_element = unsafe { old_ty.data_ptr().as_ref() }
            .array_element()
            .expect("migrating an array that is not an array");
        let length = unsafe { (*header.as_ref().data.cast::<ArrayHeader>()).length };

        let (src, dest) = self.begin(header, array_layout(new_ty, length), new_ty);
        unsafe { init_array_header(dest.as_ptr(), length) };

        let new_element_ty = new_ty.as_array().expect("Must be an array.").element_type();
        let new_element = unsafe { new_ty.data_ptr().as_ref() }
            .array_element()
            .expect("Must be an array.");
        let src_stride = old_element.reference_layout().pad_to_align().size();
        let dest_stride = new_element.reference_layout().pad_to_align().size();
        let src_elements = unsafe { src.as_ptr().add(array_data_offset(old_element)) };
        let dest_elements = unsafe { dest.as_ptr().add(array_data_offset(new_element)) };
        for index in 0..length {
            unsafe {
                self.map_type(
                    NonNull::new_unchecked(src_elements.add(index * src_stride)),
                    NonNull::new_unchecked(dest_elements.add(index * dest_stride)),
                    element_action,
                    &new_element_ty,
                );
            }
        }
    }

    fn map_struct(&mut self, mapping: &[FieldMapping], src: NonNull<u8>, dest: NonNull<u8>) {
        for FieldMapping {
            new_ty,
            new_offset,
            action,
        } in mapping.iter()
        {
            let field_dest = unsafe { NonNull::new_unchecked(dest.as_ptr().add(*new_offset)) };
            self.map_type(src, field_dest, action, new_ty);
        }
    }

    fn conversion(&self, old_ty: &Type) -> &'_ mapping::StructMapping {
        self.conversions.get(old_ty).unwrap_or_else(|| {
            panic!("If the struct changed, there must also be a conversion for type: {old_ty:#?}.")
        })
    }

    fn map_type(&mut self, src: NonNull<u8>, dest: NonNull<u8>, action: &Action, new_ty: &Type) {
        unsafe fn field(ptr: NonNull<u8>, offset: usize) -> NonNull<u8> {
            NonNull::new_unchecked(ptr.as_ptr().add(offset))
        }
        unsafe fn write_handle(dest: NonNull<u8>, header: NonNull<ObjectHeader>) {
            *dest.cast::<GcPtr>().as_ptr() = handle_of(header);
        }
        unsafe fn read_header(src: NonNull<u8>) -> Option<NonNull<ObjectHeader>> {
            NonNull::new(*src.cast::<*mut ObjectHeader>().as_ptr())
        }

        match action {
            Action::ArrayAlloc => {
                let header = self.alloc(new_ty, array_layout(new_ty, 0));
                unsafe {
                    init_array_header(header.as_ref().data, 0);
                    write_handle(dest, header);
                }
            }
            Action::ArrayFromValue {
                element_action,
                old_offset,
            } => {
                let header = self.alloc(new_ty, array_layout(new_ty, 1));
                let element_ty = new_ty.as_array().expect("Must be an array.").element_type();
                let element = unsafe { element_ty.data_ptr().as_ref() };
                unsafe {
                    let payload = header.as_ref().data;
                    init_array_header(payload, 1);
                    let data = NonNull::new_unchecked(payload.add(array_data_offset(element)));
                    self.map_type(field(src, *old_offset), data, element_action, &element_ty);
                    write_handle(dest, header);
                }
            }
            Action::ArrayMap {
                element_action,
                old_offset,
            } => unsafe {
                let src_ptr = field(src, *old_offset);
                if let Some(array) = read_header(src_ptr) {
                    self.migrate_array(array, element_action, new_ty);
                }
                std::ptr::copy_nonoverlapping(
                    src_ptr.as_ptr(),
                    dest.as_ptr(),
                    std::mem::size_of::<GcPtr>(),
                );
            },
            Action::Cast { old_offset, old_ty } => {
                // A failed cast leaves the zero-initialized value in place.
                let _ = cast::try_cast_from_to(
                    old_ty.clone(),
                    new_ty.clone(),
                    unsafe { field(src, *old_offset) },
                    dest,
                );
            }
            Action::Copy { old_offset, size } => unsafe {
                std::ptr::copy_nonoverlapping(
                    field(src, *old_offset).as_ptr(),
                    dest.as_ptr(),
                    *size,
                );
            },
            Action::ElementFromArray {
                element_action,
                old_offset,
            } => unsafe {
                if let Some(array) = read_header(field(src, *old_offset)) {
                    let payload = self.old_payload(array);
                    if (*payload.as_ptr().cast::<ArrayHeader>()).length > 0 {
                        let old_element_ty = array.as_ref().type_view();
                        let old_element = old_element_ty
                            .data_ptr()
                            .as_ref()
                            .array_element()
                            .expect("Must be an array.");
                        let data = NonNull::new_unchecked(
                            payload.as_ptr().add(array_data_offset(old_element)),
                        );
                        self.map_type(data, dest, element_action, new_ty);
                    }
                }
            },
            Action::StructAlloc => {
                let header = self.alloc(new_ty, new_ty.value_layout());
                unsafe { write_handle(dest, header) };
            }
            Action::StructMapFromGc { old_ty, old_offset } => unsafe {
                if let Some(object) = read_header(field(src, *old_offset)) {
                    let payload = self.old_payload(object);
                    let mapping =
                        &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                    self.map_struct(&*mapping, payload, dest);
                }
            },
            Action::StructMapFromValue { old_ty, old_offset } => {
                let header = self.alloc(new_ty, new_ty.value_layout());
                let mapping = &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                unsafe {
                    let payload = NonNull::new_unchecked(header.as_ref().data);
                    self.map_struct(&*mapping, field(src, *old_offset), payload);
                    write_handle(dest, header);
                }
            }
            Action::StructMapInPlace { old_ty, old_offset } => {
                let mapping = &self.conversion(old_ty).field_mapping as *const Vec<FieldMapping>;
                unsafe { self.map_struct(&*mapping, field(src, *old_offset), dest) };
            }
            Action::ZeroInitialize => {
                // The destination is already zero-initialized.
            }
        }
    }
}

impl<O> MemoryMapper for MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    #[allow(clippy::mutable_key_type)]
    fn map_memory(&self, mapping: Mapping) -> Vec<GcPtr> {
        // Rewriting objects in place while another thread runs Codira code
        // would race with it; stop the world (a no-op for the thread that
        // already holds the stop, as a reload does).
        safepoint::stop_the_world(|_| self.map_memory_stopped(mapping))
    }
}

impl<O> MarkSweep<O>
where
    O: Observer<Event = Event>,
{
    #[allow(clippy::mutable_key_type)]
    fn map_memory_stopped(&self, mapping: Mapping) -> Vec<GcPtr> {
        let mut inner = self.heap.inner.write();
        let objects = inner.objects();

        // Determine which objects still use deleted types
        let deleted = objects
            .iter()
            .filter(|o| {
                mapping
                    .deletions
                    .contains(&*unsafe { o.as_ref().type_view() })
            })
            .map(|o| handle_of(*o))
            .collect();

        // Update the types of objects whose type didn't change
        let identical: HashMap<Type, Type> = mapping.identical.iter().cloned().collect();
        for object in &objects {
            let header = object.as_ptr();
            let new_ty = identical.get(&*unsafe { (*header).type_view() });
            if let Some(new_ty) = new_ty {
                inner.register_type(new_ty);
                unsafe { (*header).ty = new_ty.data_ptr().as_ptr() };
            }
        }

        // Plan the struct and array migrations
        let mut struct_migrations = Vec::new();
        let mut array_migrations = Vec::new();
        let mut retyped_arrays = Vec::new();
        for &object in &objects {
            let ty = unsafe { object.as_ref().type_view() };
            if ty.is_struct() {
                if let Some(conversion) = mapping.struct_mappings.get(&*ty) {
                    struct_migrations.push((object, conversion));
                }
            } else if ty.is_array() {
                // Find the innermost element type
                let mut element_ty = ty.clone();
                let mut depth = 0;
                while let Some(array) = element_ty.as_array() {
                    element_ty = array.element_type();
                    depth += 1;
                }

                if let Some(conversion) = mapping.struct_mappings.get(&element_ty) {
                    let mut new_ty = conversion.new_ty.clone();
                    for _ in 0..depth {
                        new_ty = new_ty.array_type();
                    }

                    // Only arrays containing structs need to be mapped, as an array of
                    // arrays merely contains `GcPtr`s.
                    let new_element_ty = new_ty.as_array().unwrap().element_type();
                    if new_element_ty.is_struct() {
                        // Conversion between ADTs are already handled in struct mappings
                        assert!(element_ty.is_struct());
                        let element_action =
                            resolve_struct_to_struct_edit(&element_ty, &new_element_ty, 0);
                        array_migrations.push((object, element_action, new_ty));
                    } else {
                        retyped_arrays.push((object, new_ty));
                    }
                }
            }
        }

        let mut ctx = MigrationContext {
            inner: &mut inner,
            conversions: &mapping.struct_mappings,
            migrations: FxHashMap::default(),
            new_objects: Vec::new(),
        };

        // Detach every struct payload first, so that mapping one object can
        // read the old payload of any other object regardless of order.
        let mut struct_payloads = Vec::with_capacity(struct_migrations.len());
        for (object, conversion) in &struct_migrations {
            let (src, dest) = ctx.begin(
                *object,
                conversion.new_ty.value_layout(),
                &conversion.new_ty,
            );
            struct_payloads.push((src, dest, *conversion));
        }
        for (src, dest, conversion) in struct_payloads {
            ctx.map_struct(&conversion.field_mapping, src, dest);
        }
        for (object, element_action, new_ty) in &array_migrations {
            ctx.migrate_array(*object, element_action, new_ty);
        }

        // Complete the migrations: switch types and release the old payloads
        let migrations = std::mem::take(&mut ctx.migrations);
        let new_objects = std::mem::take(&mut ctx.new_objects);
        let migrated: FxHashSet<usize> = migrations.keys().copied().collect();
        for (address, (migration, new_ty)) in migrations {
            inner.register_type(&new_ty);
            unsafe { (*(address as *mut ObjectHeader)).ty = new_ty.data_ptr().as_ptr() };
            inner.finish_payload_migration(migration);
        }
        for (object, new_ty) in retyped_arrays {
            // An array whose element type changed shape (e.g. `[[T]]` to
            // `[T]`) was migrated through its owner's field mapping; that
            // migration decides its type, not the element-type rename.
            if migrated.contains(&(object.as_ptr() as usize)) {
                continue;
            }
            inner.register_type(&new_ty);
            unsafe { (*object.as_ptr()).ty = new_ty.data_ptr().as_ptr() };
        }

        // Types no live object uses anymore no longer need to be kept alive
        inner.prune_types();
        drop(inner);
        self.heap.bump_type_epoch();

        for handle in new_objects {
            self.core.observer.event(Event::Allocation(handle));
        }

        deleted
    }
}
