//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Cascade, the experimental reclamation engine (off by default): deferred
//!   ownership accounting for objects of a `MarkSweep` heap, an epoch
//!   protocol that makes deferred releases safe, a retire list, and
//!   candidate-based concurrent cycle reclamation with a bounded work budget
//!   per step. The tracing collector stays the backstop and resynchronises
//!   every count when it runs.
//!
//! The design, its invariants and its assumptions are in
//! `crates/codira_memory/CASCADE.md`; section numbers below refer to it.
//! Nothing here runs unless a host calls `MarkSweep::enable_cascade`, and
//! only objects allocated through a [`Mutator`] are tracked: every other
//! object keeps an untracked ownership word and is reclaimed by tracing
//! alone.

mod cycles;
mod epoch;
#[cfg(all(test, loom))]
mod loom_tests;
mod mutator;
mod sync;
pub mod word;

use std::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::{
        atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

pub use mutator::Mutator;
pub use word::{OwnershipState, Word};

use self::{
    cycles::{trial_deletion, CycleGraph, PassError},
    epoch::EpochCore,
    word::{BUFFERED, CYCLE, TOUCHED, ZCT},
};
use crate::{
    gc::{
        heap::{handle_of, header_of, AllocError, Heap, HeapInner, ObjectHeader, STATE_ALLOCATED},
        mark_sweep::for_each_reference_slot,
        GcPtr,
    },
    r#type::Type,
};

/// The most epoch boundaries one step completes.
const MAX_BOUNDARIES_PER_STEP: usize = 2;

/// What Cascade needs from the collector it is attached to.
pub trait CascadeHost: Send + Sync {
    /// Allocates an object (a safepoint; may collect at the heap limit).
    fn cascade_alloc(&self, ty: &Type) -> Result<GcPtr, AllocError>;

    /// Allocates an array of `n` elements of the array type `ty`.
    fn cascade_alloc_array(&self, ty: &Type, n: usize) -> Result<GcPtr, AllocError>;

    /// Runs a full tracing collection (the backstop).
    fn cascade_collect(&self) -> bool;
}

/// A callback told about every object Cascade reclaims (for tests and
/// diagnostics), called while the heap lock is held: it must not touch the
/// heap.
pub type ReclaimHook = Arc<dyn Fn(GcPtr) + Send + Sync>;

/// Tuning of a Cascade engine.
#[derive(Clone)]
pub struct CascadeConfig {
    /// Logged operations a mutator buffers before handing them over
    /// without waiting for its next handshake.
    pub buffer_capacity: usize,
    /// The work budget of the owner thread's synchronous cycle pass at
    /// every handshake.
    pub local_cycle_budget: usize,
    /// The largest concurrent cycle pass, in work units; a pass that needs
    /// more is abandoned and its garbage left to the tracing collector.
    pub max_cycle_pass: usize,
    /// Deferred bytes (retiring and candidate objects) above which a step
    /// counts as under pressure.
    pub pressure_bytes: usize,
    /// The factor by which a step's budget grows under pressure.
    pub pressure_multiplier: usize,
    /// Consecutive steps under pressure after which Cascade runs the tracing
    /// collector.
    pub pressure_steps_before_trace: usize,
    /// Called for every reclaimed object.
    pub on_reclaim: Option<ReclaimHook>,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 4096,
            local_cycle_budget: 4096,
            max_cycle_pass: 1 << 16,
            pressure_bytes: 64 << 20,
            pressure_multiplier: 8,
            pressure_steps_before_trace: 4,
            on_reclaim: None,
        }
    }
}

impl std::fmt::Debug for CascadeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CascadeConfig")
            .field("buffer_capacity", &self.buffer_capacity)
            .field("local_cycle_budget", &self.local_cycle_budget)
            .field("max_cycle_pass", &self.max_cycle_pass)
            .field("pressure_bytes", &self.pressure_bytes)
            .field("pressure_multiplier", &self.pressure_multiplier)
            .field(
                "pressure_steps_before_trace",
                &self.pressure_steps_before_trace,
            )
            .field("on_reclaim", &self.on_reclaim.is_some())
            .finish()
    }
}

/// Counters of a Cascade engine. All are totals since it was enabled,
/// except the `retiring_*`, `candidate_*` and `deferred_bytes` gauges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeStats {
    /// The current global epoch.
    pub epoch: u64,
    /// Completed epoch boundaries.
    pub boundaries: u64,
    /// Increments and decrements the collector applied.
    pub applied_incs: u64,
    pub applied_decs: u64,
    /// Objects reclaimed by `release_unique` at their last ownership point.
    pub unique_reclaimed: u64,
    /// Unpublished objects their owner reclaimed (zero count or local cycle).
    pub local_reclaimed: u64,
    /// Published objects reclaimed from the retire list.
    pub retired_reclaimed: u64,
    /// Objects reclaimed as members of validated garbage cycles (local and
    /// concurrent).
    pub cycle_reclaimed: u64,
    /// Payload bytes reclaimed by Cascade (all paths).
    pub reclaimed_bytes: u64,
    /// Concurrent cycle sets that passed / failed validation.
    pub cycle_sets_validated: u64,
    pub cycle_sets_rejected: u64,
    /// Trial-deletion passes abandoned for exceeding their budget.
    pub abandoned_passes: u64,
    /// Zero-count objects found positive again when re-examined.
    pub resurrections: u64,
    /// Zero-count objects left to tracing because the host rooted them.
    pub rooted_skips: u64,
    /// `release_unique` calls whose checks failed (fell back to deferral).
    pub unique_fallbacks: u64,
    /// Tracing collections Cascade started under memory pressure.
    pub pressure_traces: u64,
    /// Resynchronisations after tracing collections or migrations.
    pub resyncs: u64,
    /// Objects and payload bytes in the retire list.
    pub retiring_objects: usize,
    pub retiring_bytes: usize,
    /// Objects and payload bytes queued as cycle candidates.
    pub candidate_objects: usize,
    pub candidate_bytes: usize,
    /// `retiring_bytes + candidate_bytes`.
    pub deferred_bytes: usize,
}

/// What one [`Cascade::step`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepReport {
    /// Work units used (applied count changes, reclaimed objects, traversed
    /// edges).
    pub work: usize,
    /// The budget the step ran with (larger than requested under pressure).
    pub budget: usize,
    /// Epoch boundaries completed by this step.
    pub boundaries: u64,
    /// Objects reclaimed by this step.
    pub reclaimed: usize,
    /// Whether the step ran under memory pressure.
    pub pressure: bool,
    /// Whether the step ran the tracing collector.
    pub traced: bool,
    /// Wall-clock time of the step, excluding a tracing collection.
    pub nanos: u64,
}

/// One handshake's hand-over to the collector (`CASCADE.md` section 5).
pub(crate) struct Packet {
    thread: u64,
    /// The segment of `incs`, `decs` and `checks`.
    segment: u64,
    incs: Vec<usize>,
    decs: Vec<usize>,
    /// Newly published objects to examine (publication roots and zero
    /// counts).
    checks: Vec<usize>,
    /// The root snapshot (published objects held locally), if any.
    roots: Option<Vec<usize>>,
    standing: Standing,
}

/// How a packet changes the thread's standing roots (used while it is
/// quiescent).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    Keep,
    Set,
    Clear,
}

/// A mutator's thread-local bookkeeping. Only the owning thread touches it,
/// except while the world is stopped (the owner is then parked at a
/// safepoint and holds no reference into it).
pub(crate) struct Local {
    /// Locally held objects (stand-ins for stack slots) and their
    /// multiplicity.
    pub holds: FxHashMap<usize, u32>,
    pub incs: Vec<usize>,
    pub decs: Vec<usize>,
    pub checks: Vec<usize>,
    /// Unpublished objects whose count reached zero.
    pub zct: Vec<usize>,
    /// Unpublished cycle candidates.
    pub candidates: FxHashSet<usize>,
}

pub(crate) struct ThreadState {
    pub id: u64,
    local: UnsafeCell<Local>,
}

// Safety: `local` is only accessed by the owning thread while it runs and by
// a stop-the-world operation while the owner is parked (see `Local`); the
// safepoint protocol's handshake orders the two.
unsafe impl Sync for ThreadState {}
unsafe impl Send for ThreadState {}

impl ThreadState {
    /// # Safety
    ///
    /// The caller must be the owning thread, or hold the stop of a
    /// stop-the-world operation; and must not create two of these at once.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn local(&self) -> &mut Local {
        &mut *self.local.get()
    }
}

enum Phase {
    Idle,
    Boundary {
        boundary: u64,
        incs: Vec<usize>,
        decs: Vec<usize>,
        checks: Vec<usize>,
    },
}

struct PendingCycle {
    members: Vec<usize>,
    /// The last completed boundary when the set was computed.
    after: u64,
}

/// The collector's state, protected by `Cascade::collector` and only used
/// while the heap's write lock is held.
struct Collector {
    phase: Phase,
    /// Packets drained in a boundary whose operations belong to the next.
    held: Vec<Packet>,
    /// Operation decrements to apply at the next boundary.
    delayed_decs: Vec<usize>,
    /// Root-snapshot decrements to apply at the next boundary.
    root_decs: Vec<usize>,
    /// Decrements of the snapshot a resynchronisation took, applied one
    /// boundary later than `root_decs`.
    resync_root_decs: Vec<usize>,
    /// Root snapshots of quiescent threads.
    standing: FxHashMap<u64, Vec<usize>>,
    /// Checks to make at the next boundary (after a resynchronisation).
    pending_checks: Vec<usize>,
    retire: Vec<usize>,
    candidates: FxHashSet<usize>,
    pending_cycle: Option<PendingCycle>,
    completed: u64,
    pressure_steps: usize,
    retiring_bytes: usize,
    candidate_bytes: usize,
    stats: CascadeStats,
}

/// Counters updated by mutators without the collector lock.
#[derive(Default)]
struct SharedCounters {
    unique_reclaimed: AtomicU64,
    local_reclaimed: AtomicU64,
    cycle_reclaimed: AtomicU64,
    reclaimed_bytes: AtomicU64,
    unique_fallbacks: AtomicU64,
}

/// A Cascade engine attached to one heap. Create it with
/// `MarkSweep::enable_cascade`; drive it with [`Mutator`]s and
/// [`Cascade::step`].
pub struct Cascade {
    heap: Arc<Heap>,
    config: CascadeConfig,
    epochs: EpochCore<Packet>,
    threads: Mutex<Vec<Arc<ThreadState>>>,
    collector: Mutex<Collector>,
    counters: SharedCounters,
    deferred_bytes: AtomicUsize,
    next_thread: AtomicU64,
}

/// Returns the ownership word of `object`.
///
/// # Safety
///
/// `object` must be the header address of an allocated object.
#[inline]
pub(crate) unsafe fn word_of(object: usize) -> Word {
    Word((*(object as *const ObjectHeader)).ownership.load(Ordering::Relaxed))
}

/// Stores the ownership word of `object`.
///
/// # Safety
///
/// As [`word_of`], and the caller must be the word's single writer.
#[inline]
pub(crate) unsafe fn set_word(object: usize, word: Word) {
    (*(object as *const ObjectHeader))
        .ownership
        .store(word.0, Ordering::Relaxed);
}

/// Stores the ownership word with release ordering (publication).
///
/// # Safety
///
/// As [`set_word`].
#[inline]
pub(crate) unsafe fn publish_word(object: usize, word: Word) {
    (*(object as *const ObjectHeader))
        .ownership
        .store(word.0, Ordering::Release);
}

/// Calls `f` with every non-null reference stored in `object`, reading the
/// slots atomically (mutators may store into them concurrently).
///
/// # Safety
///
/// `object` must be the header address of an allocated object.
pub(crate) unsafe fn children_of(object: usize, f: &mut dyn FnMut(usize)) {
    for_each_reference_slot(&*(object as *const ObjectHeader), |slot| {
        let child = (*slot.cast::<AtomicPtr<ObjectHeader>>()).load(Ordering::Acquire);
        if !child.is_null() {
            f(child as usize);
        }
    });
}

/// Returns the payload size of `object`.
///
/// # Safety
///
/// `object` must be the header address of an allocated object.
unsafe fn payload_size(object: usize) -> usize {
    (*(object as *const ObjectHeader)).payload_size()
}

/// Returns the host root count of `object`.
///
/// # Safety
///
/// `object` must be the header address of an allocated object.
unsafe fn host_roots(object: usize) -> u32 {
    (*(object as *const ObjectHeader))
        .roots
        .load(Ordering::Acquire)
}

impl Cascade {
    pub(crate) fn new(heap: Arc<Heap>, config: CascadeConfig) -> Self {
        Self {
            heap,
            config,
            epochs: EpochCore::new(),
            threads: Mutex::new(Vec::new()),
            collector: Mutex::new(Collector {
                phase: Phase::Idle,
                held: Vec::new(),
                delayed_decs: Vec::new(),
                root_decs: Vec::new(),
                resync_root_decs: Vec::new(),
                standing: FxHashMap::default(),
                pending_checks: Vec::new(),
                retire: Vec::new(),
                candidates: FxHashSet::default(),
                pending_cycle: None,
                completed: 0,
                pressure_steps: 0,
                retiring_bytes: 0,
                candidate_bytes: 0,
                stats: CascadeStats::default(),
            }),
            counters: SharedCounters::default(),
            deferred_bytes: AtomicUsize::new(0),
            next_thread: AtomicU64::new(1),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &CascadeConfig {
        &self.config
    }

    /// Returns the ownership word of `object`. Diagnostics only: the value
    /// may be stale for an object another thread or the collector owns.
    pub fn word(&self, object: GcPtr) -> Word {
        unsafe { word_of(header_of(object).as_ptr() as usize) }
    }

    /// The bytes of objects Cascade knows or suspects to be garbage but has
    /// not reclaimed yet (retiring and candidate objects).
    pub fn deferred_bytes(&self) -> usize {
        self.deferred_bytes.load(Ordering::Relaxed)
    }

    /// The number of registered mutators.
    pub fn mutators(&self) -> usize {
        self.epochs.registered()
    }

    /// Returns the engine's counters.
    pub fn stats(&self) -> CascadeStats {
        let collector = self.collector.lock();
        let mut stats = collector.stats.clone();
        stats.epoch = self.epochs.epoch();
        stats.boundaries = collector.completed;
        stats.retiring_objects = collector.retire.len();
        stats.retiring_bytes = collector.retiring_bytes;
        stats.candidate_objects = collector.candidates.len();
        stats.candidate_bytes = collector.candidate_bytes;
        stats.deferred_bytes = collector.retiring_bytes + collector.candidate_bytes;
        let c = &self.counters;
        stats.unique_reclaimed = c.unique_reclaimed.load(Ordering::Relaxed);
        stats.local_reclaimed = c.local_reclaimed.load(Ordering::Relaxed);
        stats.cycle_reclaimed += c.cycle_reclaimed.load(Ordering::Relaxed);
        stats.reclaimed_bytes += c.reclaimed_bytes.load(Ordering::Relaxed);
        stats.unique_fallbacks = c.unique_fallbacks.load(Ordering::Relaxed);
        stats
    }

    fn report_reclaim(&self, object: usize) {
        if let Some(hook) = &self.config.on_reclaim {
            hook(handle_of(unsafe { NonNull::new_unchecked(object as *mut ObjectHeader) }));
        }
    }

    /// Releases `objects` (already marked reclaimed) to the heap.
    fn release_to_heap(&self, inner: &mut HeapInner, objects: &mut Vec<usize>) -> usize {
        if objects.is_empty() {
            return 0;
        }
        let headers: Vec<NonNull<ObjectHeader>> = objects
            .iter()
            .map(|&o| unsafe { NonNull::new_unchecked(o as *mut ObjectHeader) })
            .collect();
        for &object in objects.iter() {
            self.report_reclaim(object);
        }
        objects.clear();
        unsafe { inner.reclaim_objects(&headers) }
    }

    /// Does up to `budget` units of collector work (`CASCADE.md` section
    /// 7): continue or start an epoch boundary, reclaim retired objects,
    /// then run or validate one concurrent cycle pass. Under memory
    /// pressure the budget grows, and persistent pressure runs the tracing
    /// collector through `host`.
    pub fn step(&self, host: &dyn CascadeHost, budget: usize) -> StepReport {
        let start = Instant::now();
        let mut report = StepReport::default();
        let trace = {
            let mut inner = self.heap.inner.write();
            let mut collector = self.collector.lock();
            let pressure = {
                let deferred = collector.retiring_bytes + collector.candidate_bytes;
                let heap_tight = inner
                    .limit()
                    .is_some_and(|limit| inner.committed().saturating_add(limit / 8) > limit);
                deferred > self.config.pressure_bytes || heap_tight
            };
            let budget = if pressure {
                budget.saturating_mul(self.config.pressure_multiplier.max(1))
            } else {
                budget
            };
            report.budget = budget;
            report.pressure = pressure;
            let before = collector.completed;
            let reclaimed_before = collector.stats.retired_reclaimed
                + collector.stats.cycle_reclaimed;
            let mut freed = Vec::new();
            let mut work = 0usize;

            self.boundary_work(&mut collector, &mut work, budget);
            self.retire_work(&mut collector, &mut freed, &mut work, budget);
            self.cycle_work(&mut collector, &mut freed, &mut work, budget);
            // Children freed by the cycle pass may have retired more.
            self.retire_work(&mut collector, &mut freed, &mut work, budget);

            let bytes = self.release_to_heap(&mut inner, &mut freed);
            collector.stats.reclaimed_bytes += bytes as u64;
            report.work = work;
            report.boundaries = collector.completed - before;
            report.reclaimed = (collector.stats.retired_reclaimed
                + collector.stats.cycle_reclaimed
                - reclaimed_before) as usize;
            self.deferred_bytes.store(
                collector.retiring_bytes + collector.candidate_bytes,
                Ordering::Relaxed,
            );

            let trace = if pressure {
                collector.pressure_steps += 1;
                collector.pressure_steps >= self.config.pressure_steps_before_trace.max(1)
            } else {
                collector.pressure_steps = 0;
                false
            };
            if trace {
                collector.pressure_steps = 0;
                collector.stats.pressure_traces += 1;
            }
            trace
        };
        report.nanos = start.elapsed().as_nanos() as u64;
        if trace {
            // Both locks are released: the collection's resynchronisation
            // takes them itself.
            host.cascade_collect();
            report.traced = true;
        }
        report
    }

    /// Starts and continues epoch boundaries until the budget is used up or
    /// the epoch cannot advance.
    fn boundary_work(&self, collector: &mut Collector, work: &mut usize, budget: usize) {
        // An idle epoch costs no work units; bound how many one step
        // completes so a step always returns.
        let mut completed = 0;
        loop {
            if completed == MAX_BOUNDARIES_PER_STEP {
                return;
            }
            if matches!(collector.phase, Phase::Idle) {
                let Some((boundary, packets)) = self.epochs.try_begin_boundary() else {
                    return;
                };
                self.begin_boundary(collector, boundary, packets);
            }
            let Phase::Boundary {
                boundary,
                incs,
                decs,
                checks,
            } = &mut collector.phase
            else {
                unreachable!()
            };
            let boundary = *boundary;
            let mut incs = std::mem::take(incs);
            let mut decs = std::mem::take(decs);
            let mut checks = std::mem::take(checks);

            // All increments before any decrement.
            while *work < budget {
                let Some(object) = incs.pop() else { break };
                *work += 1;
                self.apply_inc(collector, object);
            }
            if incs.is_empty() {
                while *work < budget {
                    let Some(object) = decs.pop() else { break };
                    *work += 1;
                    self.apply_dec(collector, object);
                }
            }
            if incs.is_empty() && decs.is_empty() {
                while *work < budget {
                    let Some(object) = checks.pop() else { break };
                    *work += 1;
                    self.check(collector, object);
                }
            }
            if incs.is_empty() && decs.is_empty() && checks.is_empty() {
                collector.phase = Phase::Idle;
                collector.completed = boundary;
                self.epochs.finish_boundary(boundary);
                completed += 1;
                if *work >= budget {
                    return;
                }
                continue;
            }
            collector.phase = Phase::Boundary {
                boundary,
                incs,
                decs,
                checks,
            };
            return;
        }
    }

    fn begin_boundary(&self, collector: &mut Collector, boundary: u64, packets: Vec<Packet>) {
        let mut incs = Vec::new();
        let mut decs = std::mem::take(&mut collector.delayed_decs);
        decs.append(&mut collector.root_decs);
        let mut checks = std::mem::take(&mut collector.pending_checks);
        let mut next_delayed = Vec::new();
        let mut roots = Vec::new();

        let mut current: Vec<Packet> = std::mem::take(&mut collector.held);
        for mut packet in packets {
            // Snapshots and standing changes act now; operations of the
            // current segment wait for the next boundary.
            if let Some(snapshot) = packet.roots.take() {
                match packet.standing {
                    Standing::Set => {
                        collector.standing.insert(packet.thread, snapshot.clone());
                    }
                    Standing::Clear => {
                        collector.standing.remove(&packet.thread);
                    }
                    Standing::Keep => {}
                }
                roots.extend(snapshot);
            } else if packet.standing == Standing::Clear {
                collector.standing.remove(&packet.thread);
            }
            if packet.segment >= boundary {
                collector.held.push(packet);
            } else {
                current.push(packet);
            }
        }
        for packet in &mut current {
            incs.append(&mut packet.incs);
            next_delayed.append(&mut packet.decs);
            checks.append(&mut packet.checks);
        }
        for snapshot in collector.standing.values() {
            roots.extend_from_slice(snapshot);
        }
        collector.delayed_decs = next_delayed;
        collector.root_decs = roots.clone();
        collector.root_decs.append(&mut collector.resync_root_decs);
        incs.extend(roots);
        // `incs` and `decs` are popped from the end; order within each
        // does not matter.
        collector.phase = Phase::Boundary {
            boundary,
            incs,
            decs,
            checks,
        };
    }

    fn apply_inc(&self, collector: &mut Collector, object: usize) {
        let word = unsafe { word_of(object) };
        if !word.is_tracked() {
            debug_assert!(false, "increment of an untracked object: {word:?}");
            return;
        }
        let mut word = word.inc().set(TOUCHED);
        if matches!(word.state(), OwnershipState::Unique | OwnershipState::Borrowed) {
            word = word.with_state(OwnershipState::Shared);
        }
        unsafe { set_word(object, word) };
        collector.stats.applied_incs += 1;
    }

    fn apply_dec(&self, collector: &mut Collector, object: usize) {
        let word = unsafe { word_of(object) };
        if !word.is_tracked() {
            debug_assert!(false, "decrement of an untracked object: {word:?}");
            return;
        }
        let word = word.dec();
        unsafe { set_word(object, word) };
        collector.stats.applied_decs += 1;
        self.after_dec(collector, object, word);
    }

    /// Queues `object` after its count was decremented to `word`: retired
    /// at zero, a cycle candidate otherwise.
    fn after_dec(&self, collector: &mut Collector, object: usize, word: Word) {
        if word.count() == 0 {
            if !word.is(ZCT) {
                self.retire(collector, object, word);
            }
        } else if !word.is(BUFFERED) && !word.is(ZCT) && !word.is_sticky() {
            unsafe { set_word(object, word.set(BUFFERED)) };
            collector.candidates.insert(object);
            collector.candidate_bytes += unsafe { payload_size(object) };
        }
    }

    fn retire(&self, collector: &mut Collector, object: usize, word: Word) {
        unsafe {
            set_word(
                object,
                word.set(ZCT).with_state(OwnershipState::Retiring),
            )
        };
        if word.is(CYCLE) {
            // Queued when its cycle set is released (validation never
            // accepts a zero-count member), so it is never queued twice.
            return;
        }
        collector.retire.push(object);
        collector.retiring_bytes += unsafe { payload_size(object) };
    }

    /// Examines a newly published object: retired at zero, a cycle
    /// candidate otherwise.
    fn check(&self, collector: &mut Collector, object: usize) {
        let word = unsafe { word_of(object) };
        if word.is_tracked() {
            self.after_dec(collector, object, word);
        }
    }

    /// Reclaims objects from the retire list (`CASCADE.md` section 5).
    fn retire_work(
        &self,
        collector: &mut Collector,
        freed: &mut Vec<usize>,
        work: &mut usize,
        budget: usize,
    ) {
        while *work < budget {
            let Some(object) = collector.retire.pop() else {
                return;
            };
            *work += 1;
            let word = unsafe { word_of(object) };
            debug_assert!(word.is(ZCT), "retired object lost its flag: {word:?}");
            let size = unsafe { payload_size(object) };
            if word.count() > 0 {
                collector.retiring_bytes -= size;
                collector.stats.resurrections += 1;
                let word = word.clear(ZCT).with_state(OwnershipState::Shared);
                unsafe { set_word(object, word) };
                // As after any decrement that leaves a positive count.
                self.after_dec(collector, object, word);
            } else if word.is(CYCLE) {
                // Not reachable (`retire` does not queue cycle-set members);
                // kept as a guard: its cycle set's release queues it.
                debug_assert!(false, "a cycle-set member in the retire list");
                collector.retiring_bytes -= size;
            } else if unsafe { host_roots(object) } > 0 {
                collector.retiring_bytes -= size;
                collector.stats.rooted_skips += 1;
                unsafe {
                    set_word(
                        object,
                        word.clear(ZCT).with_state(OwnershipState::Shared),
                    )
                };
            } else {
                collector.retiring_bytes -= size;
                *work += self.free_published(collector, object, freed, None);
                collector.stats.retired_reclaimed += 1;
            }
        }
    }

    /// Marks a dead published object reclaimed and decrements its children
    /// (outside `except`). Returns the work done.
    fn free_published(
        &self,
        collector: &mut Collector,
        object: usize,
        freed: &mut Vec<usize>,
        except: Option<&FxHashSet<usize>>,
    ) -> usize {
        let word = unsafe { word_of(object) };
        if word.is(BUFFERED) && collector.candidates.remove(&object) {
            collector.candidate_bytes -= unsafe { payload_size(object) };
        }
        let mut work = 0;
        let mut children = Vec::new();
        unsafe { children_of(object, &mut |child| children.push(child)) };
        for child in children {
            work += 1;
            if except.is_some_and(|set| set.contains(&child)) {
                continue;
            }
            let child_word = unsafe { word_of(child) };
            if !child_word.is_tracked() {
                continue;
            }
            debug_assert!(
                child_word.is_published(),
                "published object references an unpublished one"
            );
            let child_word = child_word.dec();
            unsafe { set_word(child, child_word) };
            self.after_dec(collector, child, child_word);
        }
        unsafe { set_word(object, Word::UNTRACKED.with_state(OwnershipState::Reclaimed)) };
        freed.push(object);
        work
    }

    /// Validates a pending cycle set, or computes one from candidates.
    fn cycle_work(
        &self,
        collector: &mut Collector,
        freed: &mut Vec<usize>,
        work: &mut usize,
        budget: usize,
    ) {
        if !matches!(collector.phase, Phase::Idle) || *work >= budget {
            return;
        }
        if let Some(pending) = &collector.pending_cycle {
            if collector.completed <= pending.after {
                return;
            }
            let pending = collector.pending_cycle.take().expect("pending");
            self.validate_cycle(collector, pending, freed, work);
            return;
        }
        if collector.candidates.is_empty() {
            return;
        }
        let pass_budget = (budget - *work).min(self.config.max_cycle_pass);
        // Take candidates as roots.
        let take = collector.candidates.len().min(pass_budget.max(1));
        let picked: Vec<usize> = collector.candidates.iter().take(take).copied().collect();
        let mut roots = Vec::with_capacity(picked.len());
        for object in picked {
            collector.candidates.remove(&object);
            collector.candidate_bytes -= unsafe { payload_size(object) };
            let word = unsafe { word_of(object) };
            unsafe { set_word(object, word.clear(BUFFERED)) };
            if !word.is(ZCT) && !word.is(CYCLE) && word.count() > 0 {
                roots.push(object);
            }
        }
        if roots.is_empty() {
            return;
        }
        let view = PublishedView;
        let mut used = 0;
        match trial_deletion(&view, &roots, pass_budget, &mut used) {
            Ok(white) => {
                *work += used;
                if white.is_empty() {
                    return;
                }
                for &member in &white {
                    let word = unsafe { word_of(member) };
                    unsafe { set_word(member, word.set(CYCLE).clear(TOUCHED)) };
                }
                collector.pending_cycle = Some(PendingCycle {
                    members: white,
                    after: collector.completed,
                });
            }
            Err(PassError::OverBudget) => {
                *work += used;
                if pass_budget >= self.config.max_cycle_pass {
                    collector.stats.abandoned_passes += 1;
                } else {
                    // Retry with a fuller budget.
                    for object in roots {
                        let word = unsafe { word_of(object) };
                        if !word.is(BUFFERED) {
                            unsafe { set_word(object, word.set(BUFFERED)) };
                            collector.candidates.insert(object);
                            collector.candidate_bytes += unsafe { payload_size(object) };
                        }
                    }
                }
            }
        }
    }

    fn validate_cycle(
        &self,
        collector: &mut Collector,
        pending: PendingCycle,
        freed: &mut Vec<usize>,
        work: &mut usize,
    ) {
        let members = pending.members;
        let (ok, used) = cycles::validate(
            &members,
            &|member| {
                let word = unsafe { word_of(member) };
                let usable = word.is_tracked()
                    && word.is_published()
                    && word.is(CYCLE)
                    && !word.is(TOUCHED)
                    && !word.is(ZCT)
                    && !word.is_sticky()
                    && word.count() > 0
                    && unsafe { host_roots(member) } == 0;
                usable.then_some(word.count())
            },
            &|member, f| unsafe { children_of(member, f) },
        );
        *work += used;
        if ok {
            collector.stats.cycle_sets_validated += 1;
            let set: FxHashSet<usize> = members.iter().copied().collect();
            for &member in &members {
                *work += 1 + self.free_published(collector, member, freed, Some(&set));
            }
            collector.stats.cycle_reclaimed += members.len() as u64;
        } else {
            collector.stats.cycle_sets_rejected += 1;
            // One live member rejects the whole set, but the set may also
            // hold garbage cycles no later decrement would queue again:
            // members that passed both tests on their own become candidates
            // again (a live one turns black in the next pass).
            let set: FxHashSet<usize> = members.iter().copied().collect();
            let mut internal: FxHashMap<usize, u32> = FxHashMap::default();
            for &member in &members {
                unsafe {
                    children_of(member, &mut |child| {
                        if set.contains(&child) {
                            *internal.entry(child).or_default() += 1;
                        }
                    })
                };
            }
            *work += members.len();
            for &member in &members {
                let word = unsafe { word_of(member) };
                let passed = !word.is(TOUCHED)
                    && !word.is(ZCT)
                    && word.count() > 0
                    && word.count() == internal.get(&member).copied().unwrap_or(0);
                let word = word.clear(CYCLE);
                unsafe { set_word(member, word) };
                if word.is(ZCT) {
                    collector.retire.push(member);
                    collector.retiring_bytes += unsafe { payload_size(member) };
                } else if passed && !word.is(BUFFERED) {
                    unsafe { set_word(member, word.set(BUFFERED)) };
                    collector.candidates.insert(member);
                    collector.candidate_bytes += unsafe { payload_size(member) };
                }
            }
        }
    }

    /// Extra roots for a tracing collection: every object a mutator holds
    /// locally (`CASCADE.md` section 8).
    ///
    /// # Safety
    ///
    /// The world must be stopped.
    pub(crate) unsafe fn trace_roots(&self, mut f: impl FnMut(NonNull<ObjectHeader>)) {
        for thread in self.threads.lock().iter() {
            for &object in thread.local().holds.keys() {
                f(NonNull::new_unchecked(object as *mut ObjectHeader));
            }
        }
    }

    /// Resynchronises every count with the heap after a tracing collection
    /// or a migration (`CASCADE.md` section 8).
    ///
    /// # Safety
    ///
    /// The world must be stopped, and `inner` must be the heap's state.
    pub(crate) unsafe fn resync(&self, inner: &HeapInner) {
        let mut collector = self.collector.lock();
        let collector = &mut *collector;

        // Every object some dropped piece of work mentions is examined
        // again at the next boundary (if it survived): a dropped decrement
        // or snapshot decrement may have been what would have made it a
        // cycle candidate, and a conservatively retained garbage object
        // must not fall out of every queue. Only addresses are collected
        // here; objects the sweep freed are never dereferenced.
        let mut recheck: Vec<usize> = Vec::new();

        // Logged operations describe changes the heap already shows, so
        // they are dropped. Root snapshots handed over but not applied yet
        // still protect what their threads hold at the next boundary (those
        // threads will not hand over another one for it), so packets that
        // carry one are kept, emptied of operations. Held packets' snapshots
        // were already applied.
        for packet in collector.held.drain(..) {
            recheck.extend(packet.decs);
            recheck.extend(packet.checks);
        }
        let mut kept_packets = Vec::new();
        for mut packet in self.epochs.drain() {
            packet.incs.clear();
            recheck.append(&mut packet.decs);
            recheck.append(&mut packet.checks);
            if packet.roots.is_some() || packet.standing != Standing::Keep {
                kept_packets.push(packet);
            }
        }
        if let Phase::Boundary {
            boundary,
            decs,
            checks,
            ..
        } = std::mem::replace(&mut collector.phase, Phase::Idle)
        {
            recheck.extend(decs);
            recheck.extend(checks);
            collector.completed = boundary;
            self.epochs.finish_boundary(boundary);
        }
        recheck.append(&mut collector.delayed_decs);
        recheck.append(&mut collector.root_decs);
        recheck.append(&mut collector.pending_checks);
        recheck.append(&mut collector.retire);
        recheck.extend(collector.candidates.drain());
        if let Some(pending) = collector.pending_cycle.take() {
            recheck.extend(pending.members);
        }
        collector.retiring_bytes = 0;
        collector.candidate_bytes = 0;
        let threads = self.threads.lock();
        for thread in threads.iter() {
            let local = thread.local();
            local.incs.clear();
            recheck.append(&mut local.decs);
            recheck.append(&mut local.checks);
        }

        let mut tracked = Vec::new();
        let mut survivors: FxHashSet<usize> = FxHashSet::default();
        inner.for_each_object(|header| {
            let object = header.as_ptr() as usize;
            let word = word_of(object);
            if word.is_tracked() {
                let mut word = word
                    .with_count(0)
                    .clear(ZCT | BUFFERED | CYCLE | TOUCHED);
                if word.state() == OwnershipState::Retiring {
                    word = word.with_state(OwnershipState::Shared);
                }
                set_word(object, word);
                tracked.push(object);
                survivors.insert(object);
            }
        });
        for &object in &tracked {
            children_of(object, &mut |child| {
                let word = word_of(child);
                if word.is_tracked() {
                    set_word(child, word.inc());
                }
            });
        }
        // The recount applied every decrement at once, including the ones
        // the epoch scheme delays so that a thread which loaded an object
        // after its last snapshot stays covered until its next one. Restore
        // that cover with a snapshot of every thread's exact local
        // references (the world is stopped), applied now and released only
        // after the first boundary whose snapshots all postdate this point.
        recheck.append(&mut collector.resync_root_decs);
        for thread in threads.iter() {
            for &object in thread.local().holds.keys() {
                let word = word_of(object);
                if word.is_published() {
                    set_word(object, word.inc());
                    collector.resync_root_decs.push(object);
                }
            }
        }
        for &object in &tracked {
            let word = word_of(object);
            if word.is_published() && word.count() == 0 {
                collector.pending_checks.push(object);
            }
        }
        for object in recheck {
            if survivors.contains(&object) && word_of(object).is_published() {
                collector.pending_checks.push(object);
            }
        }
        // A snapshot may name an object its thread dropped since, which the
        // sweep may have freed: keep only survivors.
        for mut packet in kept_packets {
            if let Some(roots) = &mut packet.roots {
                roots.retain(|o| survivors.contains(o));
            }
            self.epochs.push(packet);
        }
        for roots in collector.standing.values_mut() {
            roots.retain(|o| survivors.contains(o));
        }

        // The owners' queues keep their surviving unpublished entries, with
        // their flags restored (the owner re-examines them).
        for thread in threads.iter() {
            let local = thread.local();
            local.zct.retain(|o| survivors.contains(o));
            local.zct.sort_unstable();
            local.zct.dedup();
            for &object in &local.zct {
                let word = word_of(object);
                if !word.is_published() {
                    set_word(object, word.set(ZCT));
                }
            }
            local.candidates.retain(|o| survivors.contains(o));
            local.candidates.retain(|&o| !word_of(o).is_published());
            for &object in &local.candidates {
                set_word(object, word_of(object).set(BUFFERED));
            }
        }
        drop(threads);
        collector.stats.resyncs += 1;
        self.deferred_bytes.store(0, Ordering::Relaxed);
    }

    fn register(&self) -> (Arc<ThreadState>, sync::Arc<epoch::Slot>) {
        let id = self.next_thread.fetch_add(1, Ordering::Relaxed);
        let state = Arc::new(ThreadState {
            id,
            local: UnsafeCell::new(Local {
                holds: FxHashMap::default(),
                incs: Vec::new(),
                decs: Vec::new(),
                checks: Vec::new(),
                zct: Vec::new(),
                candidates: FxHashSet::default(),
            }),
        });
        self.threads.lock().push(state.clone());
        let slot = self.epochs.register();
        (state, slot)
    }

    fn unregister(&self, state: &Arc<ThreadState>, slot: &sync::Arc<epoch::Slot>) {
        self.epochs.unregister(slot);
        self.threads.lock().retain(|t| !Arc::ptr_eq(t, state));
    }
}

/// The concurrent pass's view: published, allocated, counted objects; a
/// host root counts as one external reference.
struct PublishedView;

impl CycleGraph for PublishedView {
    fn count(&self, object: usize) -> Option<u32> {
        let header = object as *const ObjectHeader;
        if unsafe { (*header).state.load(Ordering::Acquire) } != STATE_ALLOCATED {
            return None;
        }
        let word = unsafe { word_of(object) };
        let usable = word.is_tracked()
            && word.is_published()
            && !word.is(ZCT)
            && !word.is(CYCLE)
            && !word.is_sticky()
            && word.count() > 0;
        usable.then(|| word.count() + u32::from(unsafe { host_roots(object) } > 0))
    }

    fn children(&self, object: usize, f: &mut dyn FnMut(usize)) {
        unsafe { children_of(object, f) }
    }
}
