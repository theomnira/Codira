//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Stop-the-world coordination between the collector and every thread that
//!   runs Codira code: mutator registration, safepoints, blocking regions,
//!   and the conservative scan of parked threads' stacks.
//!
//! # Why
//!
//! Generated code has no stack maps. The references it holds live in stack
//! slots and callee-saved registers the collector cannot identify, so the
//! only safe way to collect is to find every thread that may hold such
//! references, stop it at a point where its stack is quiescent, and treat
//! every word of that stack as a potential reference (a conservative scan;
//! objects never move, so a false positive only retains garbage).
//!
//! # Protocol
//!
//! A thread is a *mutator* while it holds a [`MutatorGuard`] ([`enter`]):
//! the runtime takes one around every call into Codira code (host calls,
//! worker-pool tasks). A mutator is either *running* (it may touch the GC
//! heap and hold unrooted references in registers) or *parked* (it does
//! neither, and the part of its stack that holds Codira frames is frozen).
//!
//! A collection ([`stop_the_world`]) sets a global stop flag and waits until
//! no thread is running. Running mutators notice the flag at their next
//! *safepoint* -- every GC allocation ([`poll`]) -- and park. Waits in the
//! runtime (channel operations, scope joins, sleeps) run inside a
//! [`blocking`] region, which parks the thread for their duration, so a
//! thread blocked on a channel never holds up a collection and cannot
//! deadlock it. When a parked thread wants to run again while a stop is in
//! progress, it waits for the collection to end first.
//!
//! Parking records the thread's stack range: from a word in the frame of
//! [`blocking`] (below every Codira frame, and below the slots where the
//! callee-saved registers were spilled) up to the base of the thread's
//! stack. Those frames cannot change while the thread is parked, because the
//! thread only runs code deeper on the stack until it is running again.
//!
//! A thread that starts running stores its per-thread `running` flag and
//! then reads the global stop flag, both `SeqCst`; a collector stores the
//! stop flag and then reads every `running` flag, both `SeqCst` (a
//! Dekker-style handshake), so the two never both proceed. Stopping only
//! needs a release store. The allocation fast path costs a thread-local read
//! and one relaxed load.
//!
//! # What this does not cover
//!
//! - Code that calls Codira functions through raw function pointers without
//!   holding a [`MutatorGuard`]: the collector does not know about the
//!   thread. The runtime's own entry points take the guard.
//! - A running mutator that never reaches a safepoint (a loop that neither
//!   allocates, nor blocks, nor returns) delays a collection until it does.
//! - A host function called from Codira code runs as part of the mutator;
//!   if it blocks on something only the collecting thread can release, the
//!   two wait for each other. Such waits belong in [`blocking`].
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use parking_lot::{const_mutex, Condvar, Mutex};

use super::{
    frames::{self, ParkedStack, StackRoots},
    stack_scan,
};

/// Set while a collection (or another stop-the-world operation) is
/// requested or in progress.
static STOP: AtomicBool = AtomicBool::new(false);

/// The registered threads, and whether some thread holds the stop.
static REGISTRY: Mutex<Registry> = const_mutex(Registry {
    records: Vec::new(),
    stopped: false,
});

/// Signalled whenever a thread parks while a stop is requested, and when a
/// stop ends.
static CHANGED: Condvar = Condvar::new();

/// How often a collector waiting for threads to park re-checks without
/// being woken.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(1);

struct Registry {
    records: Vec<Arc<Record>>,
    /// A thread holds the stop (see [`stop_the_world`]).
    stopped: bool,
}

/// What the collector knows about one thread.
struct Record {
    /// The thread is running: it may touch the GC heap. Only the thread
    /// itself writes it.
    running: AtomicBool,
    /// The thread has Codira frames on its stack (holds a guard).
    active: AtomicBool,
    /// While the thread is parked in a [`blocking`] region: the lowest
    /// address of the frozen part of its stack; [`UNSCANNABLE`] if the
    /// platform cannot capture it; 0 otherwise.
    low: AtomicUsize,
    /// The base (highest address) of the thread's stack, 0 if unknown.
    high: usize,
    /// The address of the thread's frame-record chain head
    /// (`frames::codira_gc_chain_slot`).
    chain: usize,
    /// The thread parked at a poll of compiled code: its frame records'
    /// spill words need not be up to date, so its whole stack is scanned
    /// conservatively. Only the thread itself writes it, while running.
    at_poll: AtomicBool,
    /// While the thread is in a foreign call (`enter_foreign`): the
    /// callee-saved registers at the transition, which may hold the calling
    /// frames' references. Only the thread itself writes them, while
    /// running; the collector reads them while it is parked.
    foreign_registers: std::cell::UnsafeCell<[usize; 16]>,
    /// `low` before `enter_foreign`.
    foreign_previous_low: AtomicUsize,
    /// The thread is in a foreign call.
    in_foreign: AtomicBool,
}

// Safety: `foreign_registers` is written only by the owning thread while it
// is running and read only by a collector while that thread is parked (the
// handshake orders the two).
unsafe impl Sync for Record {}

/// [`Record::low`] of a thread parked on a platform whose stacks cannot be
/// scanned.
const UNSCANNABLE: usize = 1;

/// [`MODE`]: the thread is not running (not a mutator, or parked).
const IDLE: u8 = 0;
/// [`MODE`]: the thread is a running mutator.
const RUNNING: u8 = 1;
/// [`MODE`]: the thread holds the stop; everyone else is parked.
const OWNER: u8 = 2;

thread_local! {
    /// The thread's state, readable without lazy initialization.
    static MODE: Cell<u8> = const { Cell::new(IDLE) };
    /// Nesting of [`MutatorGuard`]s on this thread.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    /// This thread's record, registered on first use and unregistered when
    /// the thread exits.
    static HOLDER: RefCell<Option<Holder>> = const { RefCell::new(None) };
    /// The record `HOLDER` owns, for access without borrowing or reference
    /// counting; null until registration and once `HOLDER` is dropped.
    static CURRENT: Cell<*const Record> = const { Cell::new(std::ptr::null()) };
}

/// Owns a thread's registration.
struct Holder(Arc<Record>);

impl Drop for Holder {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(std::ptr::null()));
        REGISTRY
            .lock()
            .records
            .retain(|record| !Arc::ptr_eq(record, &self.0));
    }
}

/// Returns the current thread's record, registering the thread on first
/// use. `None` while the thread is shutting down.
///
/// The reference is only used on this thread, which keeps the record alive
/// (through `HOLDER`) until its thread-local storage is destroyed.
#[inline]
fn record() -> Option<&'static Record> {
    let current = CURRENT.with(Cell::get);
    if current.is_null() {
        register()
    } else {
        // Safety: `CURRENT` points to the record `HOLDER` owns and is reset
        // before `HOLDER` releases it.
        Some(unsafe { &*current })
    }
}

#[cold]
fn register() -> Option<&'static Record> {
    let record = registered_or_new()?;
    let pointer = Arc::as_ptr(&record);
    CURRENT.with(|current| current.set(pointer));
    // Safety: as in `record`; `HOLDER` holds another reference.
    Some(unsafe { &*pointer })
}

fn registered_or_new() -> Option<Arc<Record>> {
    HOLDER
        .try_with(|holder| {
            let mut holder = holder.try_borrow_mut().ok()?;
            if holder.is_none() {
                let record = Arc::new(Record {
                    running: AtomicBool::new(false),
                    active: AtomicBool::new(false),
                    low: AtomicUsize::new(0),
                    high: stack_scan::stack_base().unwrap_or(0),
                    chain: frames::chain_slot_address(),
                    at_poll: AtomicBool::new(false),
                    foreign_registers: std::cell::UnsafeCell::new([0; 16]),
                    foreign_previous_low: AtomicUsize::new(0),
                    in_foreign: AtomicBool::new(false),
                });
                REGISTRY.lock().records.push(record.clone());
                *holder = Some(Holder(record));
            }
            holder.as_ref().map(|holder| holder.0.clone())
        })
        .ok()
        .flatten()
}

/// Sets the poll word compiled code checks: a stop is requested, or stress
/// mode is on.
pub(crate) fn refresh_poll_word() {
    let on = STOP.load(Ordering::SeqCst) || frames::stress();
    frames::POLL_WORD.store(u8::from(on), Ordering::SeqCst);
}

/// Sets or clears the stop flag, and the poll word with it.
fn set_stop(on: bool) {
    STOP.store(on, Ordering::SeqCst);
    refresh_poll_word();
}

/// Waits, with the registry locked, until no stop is in progress.
fn wait_for_stop_to_end(registry: &mut parking_lot::MutexGuard<'_, Registry>) {
    while STOP.load(Ordering::SeqCst) {
        CHANGED.wait(registry);
    }
}

/// Makes the current thread running, waiting first for any collection in
/// progress to end.
fn make_running(record: &Record) {
    loop {
        record.running.store(true, Ordering::SeqCst);
        if !STOP.load(Ordering::SeqCst) {
            MODE.with(|mode| mode.set(RUNNING));
            return;
        }
        // A stop is in progress: step back and wait for it to end.
        record.running.store(false, Ordering::SeqCst);
        let mut registry = REGISTRY.lock();
        CHANGED.notify_all();
        wait_for_stop_to_end(&mut registry);
    }
}

/// Makes the current thread stop running, telling a waiting collector.
///
/// The store only needs release ordering (what the thread wrote before it,
/// its stack range included, must be visible to a collector that sees it).
/// Without a full fence the check of the stop flag below may miss a stop
/// that was just requested, so the collector does not rely on being woken:
/// it re-checks every [`STOP_POLL`].
#[inline]
fn make_idle(record: &Record) {
    MODE.with(|mode| mode.set(IDLE));
    record.running.store(false, Ordering::Release);
    if STOP.load(Ordering::Relaxed) {
        let _registry = REGISTRY.lock();
        CHANGED.notify_all();
    }
}

/// Marks the current thread as running Codira code until the guard is
/// dropped. Guards nest. While a thread holds a guard and is not inside a
/// [`blocking`] region, a collection waits for it to reach a safepoint, and
/// then scans its stack.
///
/// Take a guard around every call into Codira code that does not go
/// through the runtime (which takes one itself), and while the host holds
/// unrooted references it needs to survive a collection on another thread.
pub fn enter() -> MutatorGuard {
    let mode = MODE.with(Cell::get);
    let record = record();
    if mode == IDLE {
        match &record {
            Some(record) => make_running(record),
            None => {
                // The thread is shutting down: it cannot be scanned, so it
                // must not run while a collection might. Nothing calls into
                // Codira code from a thread-local destructor; refuse loudly.
                panic!("cannot enter Codira code while the thread is exiting");
            }
        }
    }
    let depth = DEPTH.with(|depth| {
        let value = depth.get() + 1;
        depth.set(value);
        value
    });
    if depth == 1 {
        if let Some(record) = &record {
            record.active.store(true, Ordering::Relaxed);
        }
    }
    MutatorGuard {
        resumed: mode == IDLE,
        _not_send: PhantomData,
    }
}

/// Keeps the current thread registered as running Codira code; see
/// [`enter`].
#[must_use = "the thread stops counting as running Codira code when the guard is dropped"]
pub struct MutatorGuard {
    /// Whether [`enter`] made the thread running (it was idle or parked),
    /// so dropping the guard parks it again.
    resumed: bool,
    _not_send: PhantomData<*const ()>,
}

impl Drop for MutatorGuard {
    fn drop(&mut self) {
        let depth = DEPTH.with(|depth| {
            let value = depth.get() - 1;
            depth.set(value);
            value
        });
        let record = record();
        if let Some(record) = &record {
            if depth == 0 {
                record.active.store(false, Ordering::Relaxed);
            }
            if self.resumed {
                make_idle(record);
            }
        }
    }
}

/// A safepoint: if a collection is waiting, the current (running) thread
/// parks until it is over. Called on every GC allocation.
#[inline]
pub(crate) fn poll() {
    if STOP.load(Ordering::Relaxed) && MODE.with(Cell::get) == RUNNING {
        park();
    }
}

#[cold]
#[inline(never)]
fn park() {
    blocking(|| {});
}

/// A poll of compiled code (a loop back-edge or a function entry; see
/// `frames::codira_gc_poll`): parks if a collection is waiting, collects in
/// stress mode. The thread's stack is scanned conservatively throughout
/// while it is parked here.
#[cold]
#[inline(never)]
pub(crate) fn poll_from_code() {
    if MODE.with(Cell::get) != RUNNING {
        return;
    }
    let Some(record) = record() else {
        return;
    };
    let previous = record.at_poll.swap(true, Ordering::Relaxed);
    /// Clears the flag even if the collection unwinds.
    struct Clear<'a>(&'a Record, bool);
    impl Drop for Clear<'_> {
        fn drop(&mut self) {
            self.0.at_poll.store(self.1, Ordering::Relaxed);
        }
    }
    let _clear = Clear(record, previous);
    if frames::stress_due() && frames::stress_collect_all() {
        return;
    }
    if STOP.load(Ordering::Relaxed) {
        park();
    }
}

/// Runs `f` with the current thread parked: a collection may run while `f`
/// runs, scanning this thread's stack above this call. `f` must not touch
/// the GC heap or Codira values, which is what makes it the right place for
/// waits (channel operations, joins, sleeps, host I/O) -- a thread waiting
/// inside `blocking` never holds up a collection. When `f` returns while a
/// collection is in progress, the thread waits for it to end.
///
/// Locks taken inside `f` must be released before it returns, or a running
/// thread waiting for one could hold up a collection this thread then
/// waits for.
///
/// Does nothing special on a thread that is not running Codira code.
#[inline(never)]
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    if MODE.with(Cell::get) != RUNNING {
        return f();
    }
    let Some(record) = record() else {
        return f();
    };

    // Callee-saved registers may hold references of the Codira frames
    // above; spill them into this frame, which stays put while `f` runs.
    let mut registers = [0usize; 16];
    let captured = stack_scan::capture_callee_saved_registers(&mut registers);
    let word = std::mem::size_of::<usize>();
    let low = if captured && record.high != 0 {
        (std::ptr::addr_of!(registers) as usize) & !(word - 1)
    } else {
        UNSCANNABLE
    };
    let previous = record.low.swap(low, Ordering::Relaxed);
    make_idle(record);

    /// Resumes the thread even when `f` unwinds.
    struct Resume<'a> {
        record: &'a Record,
        previous: usize,
    }
    impl Drop for Resume<'_> {
        fn drop(&mut self) {
            make_running(self.record);
            self.record.low.store(self.previous, Ordering::Relaxed);
        }
    }
    let resume = Resume {
        record: &record,
        previous,
    };
    let result = f();
    drop(resume);
    std::hint::black_box(&registers);
    result
}

/// Parks the current thread for a call into foreign code: until the
/// matching [`leave_foreign`], a collection runs without waiting for it,
/// scanning its stack from `low` (the stack pointer of the calling frame at
/// the call) up, plus the callee-saved registers at this transition.
///
/// The foreign code must not touch the GC heap or Codira values, nor call
/// back into Codira code except through [`enter`] (which makes the thread
/// running again for the duration). Compiled code brackets calls to
/// `extern` functions declared `@noalloc` with this pair; does nothing on a
/// thread that is not running Codira code.
pub fn enter_foreign(low: usize) {
    if MODE.with(Cell::get) != RUNNING {
        return;
    }
    let Some(record) = record() else {
        return;
    };
    if record.in_foreign.load(Ordering::Relaxed) || record.high == 0 || low == 0 {
        return;
    }
    let mut registers = [0usize; 16];
    if !stack_scan::capture_callee_saved_registers(&mut registers) {
        return;
    }
    // Safety: only this thread writes the registers, and no collector
    // reads them while it is running.
    unsafe { *record.foreign_registers.get() = registers };
    let word = std::mem::size_of::<usize>();
    let previous = record.low.swap(low & !(word - 1), Ordering::Relaxed);
    record.foreign_previous_low.store(previous, Ordering::Relaxed);
    record.in_foreign.store(true, Ordering::Relaxed);
    make_idle(record);
}

/// Ends the foreign call [`enter_foreign`] began: waits for a collection in
/// progress to end, and makes the thread running again.
pub fn leave_foreign() {
    let Some(record) = record() else {
        return;
    };
    if !record.in_foreign.load(Ordering::Relaxed) || MODE.with(Cell::get) != IDLE {
        return;
    }
    make_running(record);
    record.in_foreign.store(false, Ordering::Relaxed);
    record
        .low
        .store(record.foreign_previous_low.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// `codira.gc.foreign_enter`: see [`enter_foreign`].
pub extern "C" fn codira_gc_foreign_enter(low: *const u8) {
    enter_foreign(low as usize);
}

/// `codira.gc.foreign_leave`: see [`leave_foreign`].
pub extern "C" fn codira_gc_foreign_leave() {
    leave_foreign();
}

/// What a stop-the-world operation sees: every other thread is parked.
pub(crate) struct World {
    /// When this thread asked the others to stop (after any stop already in
    /// progress had ended).
    pub(crate) requested_at: std::time::Instant,
    /// When every other thread had parked.
    pub(crate) stopped_at: std::time::Instant,
    /// The parked threads' stacks.
    stacks: Vec<ParkedStack>,
    /// The callee-saved registers of threads parked in a foreign call.
    foreign_registers: Vec<usize>,
    /// Some thread holds Codira frames that cannot be scanned.
    unscannable: bool,
    /// Some thread other than the current one has Codira frames.
    others_in_codira: bool,
    /// The current thread has Codira frames.
    self_in_codira: bool,
}

impl World {
    fn snapshot(registry: &Registry, requested_at: std::time::Instant) -> Self {
        // Called with the registry locked: look at the current thread's
        // record without registering it.
        let current = CURRENT.with(Cell::get);
        let mut world = World {
            requested_at,
            stopped_at: std::time::Instant::now(),
            stacks: Vec::new(),
            foreign_registers: Vec::new(),
            unscannable: false,
            others_in_codira: false,
            self_in_codira: false,
        };
        for record in &registry.records {
            let is_self = std::ptr::eq(current, Arc::as_ptr(record));
            let active = record.active.load(Ordering::Relaxed);
            if active {
                if is_self {
                    world.self_in_codira = true;
                } else {
                    world.others_in_codira = true;
                }
            }
            match record.low.load(Ordering::Relaxed) {
                0 => world.unscannable |= active,
                UNSCANNABLE => world.unscannable = true,
                low => {
                    if record.in_foreign.load(Ordering::Relaxed) {
                        // Safety: the thread is parked in a foreign call, so
                        // it does not write them.
                        world
                            .foreign_registers
                            .extend_from_slice(unsafe { &*record.foreign_registers.get() });
                    }
                    world.stacks.push(ParkedStack {
                    low,
                    high: record.high,
                    // Safety: `chain` is the address of the thread's chain
                    // head, which lives as long as the thread, and the
                    // thread is registered (so alive) and parked.
                    head: unsafe { *(record.chain as *const *const frames::FrameRecord) },
                    conservative: record.at_poll.load(Ordering::Relaxed),
                    })
                }
            }
        }
        world
    }

    /// Returns true if some thread has Codira frames that cannot be scanned:
    /// collecting now could free objects only that thread refers to.
    pub(crate) fn has_unscannable_threads(&self) -> bool {
        self.unscannable
    }

    /// Returns true if some thread other than the current one has Codira
    /// frames on its stack.
    #[cfg(test)]
    pub(crate) fn others_in_codira(&self) -> bool {
        self.others_in_codira
    }

    /// Returns the roots of every parked thread's frozen stack range
    /// (callee-saved registers included: they were spilled into it): the
    /// slots of compiled frames' records, read precisely, and every other
    /// word, read conservatively. Empty when no thread is parked with
    /// Codira frames.
    pub(crate) fn stack_roots(&self) -> StackRoots {
        let mode = frames::root_mode();
        let mut roots = StackRoots::default();
        for stack in &self.stacks {
            // Safety: `[low, high)` is the part of a parked thread's stack
            // that holds its caller frames, and its frame records are in it.
            // The thread is blocked below it until the stop ends, so the
            // memory is mapped and nothing writes it.
            unsafe { stack.roots(mode, &mut roots) };
        }
        roots
            .conservative
            .extend_from_slice(&self.foreign_registers);
        roots
    }

    /// Every word the conservative scan of the old protocol saw: both
    /// kinds of roots.
    #[cfg(test)]
    pub(crate) fn stack_words(&self) -> Vec<usize> {
        let roots = self.stack_roots();
        let mut words = roots.conservative;
        words.extend(roots.precise);
        words
    }
}

/// Stops every thread running Codira code at a safepoint, runs `f` with the
/// world stopped, and resumes them. The current thread parks first if it is
/// running Codira code, so its own Codira frames are scanned like any
/// other thread's. Reentrant on the thread that holds the stop.
pub(crate) fn stop_the_world<R>(f: impl FnOnce(&World) -> R) -> R {
    if MODE.with(Cell::get) == OWNER {
        let world = World::snapshot(&REGISTRY.lock(), std::time::Instant::now());
        return f(&world);
    }
    blocking(|| {
        let mut registry = REGISTRY.lock();
        while registry.stopped {
            CHANGED.wait(&mut registry);
        }
        registry.stopped = true;
        set_stop(true);
        let requested_at = std::time::Instant::now();
        while registry
            .records
            .iter()
            .any(|record| record.running.load(Ordering::SeqCst))
        {
            // Woken when a thread parks; the timeout covers a wake-up a
            // thread could not know was needed (see `make_idle`).
            CHANGED.wait_for(&mut registry, STOP_POLL);
        }
        let world = World::snapshot(&registry, requested_at);
        drop(registry);

        /// Ends the stop even if `f` unwinds.
        struct Release(u8);
        impl Drop for Release {
            fn drop(&mut self) {
                MODE.with(|mode| mode.set(self.0));
                let mut registry = REGISTRY.lock();
                registry.stopped = false;
                set_stop(false);
                CHANGED.notify_all();
            }
        }
        let release = Release(MODE.with(|mode| mode.replace(OWNER)));
        let result = f(&world);
        drop(release);
        result
    })
}

/// Runs `f` with the world stopped, but only if no thread -- the current one
/// included -- is inside Codira code; otherwise returns `None` without
/// running it. While `f` runs, a thread that tries to enter Codira code
/// waits. This is what replacing code and migrating the heap needs: no
/// frame of the old code may be live anywhere.
pub fn exclusive<R>(f: impl FnOnce() -> R) -> Option<R> {
    stop_the_world(|world| (!world.others_in_codira && !world.self_in_codira).then(f))
}

/// Returns true if some thread other than the current one has Codira frames
/// on its stack (holds a [`MutatorGuard`]), whether it is running or parked.
pub(crate) fn other_threads_in_codira() -> bool {
    let registry = REGISTRY.lock();
    let current = CURRENT.with(Cell::get);
    registry.records.iter().any(|record| {
        !std::ptr::eq(current, Arc::as_ptr(record)) && record.active.load(Ordering::Relaxed)
    })
}

/// Returns true if the current thread holds the stop (it is collecting or
/// migrating): it must not start another collection from an allocation.
#[inline]
pub(crate) fn holds_stop() -> bool {
    MODE.with(Cell::get) == OWNER
}

/// Returns true if the current thread is a running mutator.
#[inline]
pub(crate) fn is_running() -> bool {
    MODE.with(Cell::get) == RUNNING
}

/// A thread's mutator state, saved by [`checkpoint`] so a non-local exit
/// (a hardware fault resumed at a checkpoint, skipping the destructors of
/// the guards and regions in between) can put it back with [`restore`].
#[derive(Clone, Copy, Debug)]
pub struct ThreadCheckpoint {
    mode: u8,
    depth: usize,
    low: usize,
    chain: *const frames::FrameRecord,
}

/// Saves the current thread's mutator state; see [`ThreadCheckpoint`].
pub fn checkpoint() -> ThreadCheckpoint {
    ThreadCheckpoint {
        mode: MODE.with(Cell::get),
        depth: DEPTH.with(Cell::get),
        low: record().map_or(0, |record| record.low.load(Ordering::Relaxed)),
        chain: frames::chain_head(),
    }
}

/// Puts the current thread's mutator state back to `checkpoint`, after
/// control returned to the point where it was taken without unwinding.
///
/// # Safety
///
/// Every guard and region entered on this thread since the checkpoint was
/// taken must have been abandoned (its frame no longer exists) and must
/// never be dropped.
pub unsafe fn restore(checkpoint: ThreadCheckpoint) {
    let Some(record) = record() else {
        return;
    };
    let mode = MODE.with(Cell::get);
    if mode == OWNER && checkpoint.mode != OWNER {
        // The stop was abandoned with the frame that held it.
        MODE.with(|mode| mode.set(IDLE));
        let mut registry = REGISTRY.lock();
        registry.stopped = false;
        set_stop(false);
        CHANGED.notify_all();
    }
    let mode = MODE.with(Cell::get);
    match (mode, checkpoint.mode) {
        (RUNNING, IDLE) => make_idle(record),
        (IDLE, RUNNING) => make_running(record),
        _ => {}
    }
    DEPTH.with(|depth| depth.set(checkpoint.depth));
    record.active.store(checkpoint.depth > 0, Ordering::Relaxed);
    record.low.store(checkpoint.low, Ordering::Relaxed);
    // A checkpoint is never taken inside a foreign call.
    record.in_foreign.store(false, Ordering::Relaxed);
    // The frames whose records were linked since are gone.
    frames::set_chain_head(checkpoint.chain);
}

/// Runs `f` with the registry locked, so no stop can begin while it runs;
/// waits for a stop in progress to end first. For threads that cannot
/// register (they are exiting) but still allocate.
pub(crate) fn without_stop<R>(f: impl FnOnce() -> R) -> R {
    let mut registry = REGISTRY.lock();
    while registry.stopped {
        CHANGED.wait(&mut registry);
    }
    f()
}

/// Brackets one allocation by a thread that is not a running mutator, so a
/// collection cannot run while it touches the heap.
pub(crate) fn with_allocation_section<R>(f: impl FnOnce() -> R) -> R {
    match record() {
        Some(_) => {
            let _guard = AllocationSection::begin();
            f()
        }
        None => without_stop(f),
    }
}

/// A brief running section for an allocation by a non-mutator thread. Does
/// not mark the thread as having Codira frames.
struct AllocationSection {
    record: &'static Record,
}

impl AllocationSection {
    fn begin() -> Self {
        let record = record().expect("checked by the caller");
        make_running(record);
        Self { record }
    }
}

impl Drop for AllocationSection {
    fn drop(&mut self) {
        make_idle(self.record);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier,
    };

    use super::*;

    /// The registry is process-wide: these tests must not see each other's
    /// threads.
    static SERIAL: Mutex<()> = const_mutex(());

    #[test]
    fn a_running_thread_holds_up_a_stop_until_its_safepoint() {
        let _serial = SERIAL.lock();
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(AtomicBool::new(false));
        let polled = Arc::new(AtomicUsize::new(0));
        let worker = {
            let (started, release, polled) = (started.clone(), release.clone(), polled.clone());
            std::thread::spawn(move || {
                let _guard = enter();
                started.wait();
                while !release.load(Ordering::SeqCst) {
                    std::hint::spin_loop();
                }
                poll();
                polled.fetch_add(1, Ordering::SeqCst);
            })
        };
        started.wait();
        let stopper = std::thread::spawn(move || {
            stop_the_world(|world| {
                // The worker parked at its safepoint, inside Codira code.
                assert!(world.others_in_codira());
                assert!(!world.stack_words().is_empty());
                polled.load(Ordering::SeqCst)
            })
        });
        // Let the worker reach its safepoint only once the stop is requested.
        while !STOP.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        release.store(true, Ordering::SeqCst);
        assert_eq!(stopper.join().unwrap(), 0, "ran before the worker parked");
        worker.join().unwrap();
    }

    #[test]
    fn a_blocked_thread_does_not_hold_up_a_stop() {
        let _serial = SERIAL.lock();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _guard = enter();
            let marker = std::hint::black_box(0x0bad_cafe_d00d_usize);
            blocking(|| rx.recv().unwrap());
            std::hint::black_box(marker);
        });
        // The worker may not be blocked yet; the stop then waits for it.
        let seen = loop {
            let seen = stop_the_world(|world| {
                world.others_in_codira() && world.stack_words().contains(&0x0bad_cafe_d00d)
            });
            if seen {
                break seen;
            }
            std::thread::yield_now();
        };
        assert!(seen);
        assert!(exclusive(|| ()).is_none(), "a thread is inside Codira code");
        tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(exclusive(|| ()).is_some());
    }

    #[test]
    fn guards_nest_and_unwind() {
        let _serial = SERIAL.lock();
        let outer = enter();
        assert!(is_running());
        {
            let _inner = enter();
            blocking(|| assert!(!is_running()));
            assert!(is_running());
        }
        assert!(is_running());
        drop(outer);
        assert!(!is_running());
    }
}
