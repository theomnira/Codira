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
//! Every word is ordered through `SeqCst` accesses of the per-thread
//! `running` flag and the global stop flag (a Dekker-style handshake), so the
//! allocation fast path costs a thread-local read and one relaxed load.
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

use super::stack_scan;

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
}

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
}

/// Owns a thread's registration.
struct Holder(Arc<Record>);

impl Drop for Holder {
    fn drop(&mut self) {
        REGISTRY
            .lock()
            .records
            .retain(|record| !Arc::ptr_eq(record, &self.0));
    }
}

/// Returns the current thread's record, registering the thread on first
/// use. `None` while the thread is shutting down.
fn record() -> Option<Arc<Record>> {
    HOLDER
        .try_with(|holder| {
            let mut holder = holder.try_borrow_mut().ok()?;
            if holder.is_none() {
                let record = Arc::new(Record {
                    running: AtomicBool::new(false),
                    active: AtomicBool::new(false),
                    low: AtomicUsize::new(0),
                    high: stack_scan::stack_base().unwrap_or(0),
                });
                REGISTRY.lock().records.push(record.clone());
                *holder = Some(Holder(record));
            }
            holder.as_ref().map(|holder| holder.0.clone())
        })
        .ok()
        .flatten()
}

/// Returns the current thread's record if it has one, without registering
/// the thread.
fn registered_record() -> Option<Arc<Record>> {
    HOLDER
        .try_with(|holder| {
            let holder = holder.try_borrow().ok()?;
            holder.as_ref().map(|holder| holder.0.clone())
        })
        .ok()
        .flatten()
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
fn make_idle(record: &Record) {
    MODE.with(|mode| mode.set(IDLE));
    record.running.store(false, Ordering::SeqCst);
    if STOP.load(Ordering::SeqCst) {
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
    make_idle(&record);

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

/// What a stop-the-world operation sees: every other thread is parked.
pub(crate) struct World {
    /// The stack ranges `[low, high)` of the parked threads.
    stacks: Vec<(usize, usize)>,
    /// Some thread holds Codira frames that cannot be scanned.
    unscannable: bool,
    /// Some thread other than the current one has Codira frames.
    others_in_codira: bool,
    /// The current thread has Codira frames.
    self_in_codira: bool,
}

impl World {
    fn snapshot(registry: &Registry) -> Self {
        // Called with the registry locked: look at the current thread's
        // record without registering it.
        let current = registered_record();
        let mut world = World {
            stacks: Vec::new(),
            unscannable: false,
            others_in_codira: false,
            self_in_codira: false,
        };
        for record in &registry.records {
            let is_self = current.as_ref().is_some_and(|c| Arc::ptr_eq(c, record));
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
                low => world.stacks.push((low, record.high)),
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

    /// Returns every pointer-aligned word of every parked thread's frozen
    /// stack range (callee-saved registers included: they were spilled into
    /// it). Empty when no thread is parked with Codira frames.
    pub(crate) fn stack_words(&self) -> Vec<usize> {
        let word = std::mem::size_of::<usize>();
        let total: usize = self
            .stacks
            .iter()
            .map(|(low, high)| high.saturating_sub(*low) / word)
            .sum();
        let mut words = Vec::with_capacity(total);
        for &(low, high) in &self.stacks {
            let mut address = low;
            while address + word <= high {
                // Safety: `[low, high)` is the part of a parked thread's
                // stack that holds its caller frames. The thread is blocked
                // below it until the stop ends, so the memory is mapped and
                // nothing writes it.
                words.push(unsafe { std::ptr::read_volatile(address as *const usize) });
                address += word;
            }
        }
        words
    }
}

/// Stops every thread running Codira code at a safepoint, runs `f` with the
/// world stopped, and resumes them. The current thread parks first if it is
/// running Codira code, so its own Codira frames are scanned like any
/// other thread's. Reentrant on the thread that holds the stop.
pub(crate) fn stop_the_world<R>(f: impl FnOnce(&World) -> R) -> R {
    if MODE.with(Cell::get) == OWNER {
        let world = World::snapshot(&REGISTRY.lock());
        return f(&world);
    }
    blocking(|| {
        let mut registry = REGISTRY.lock();
        while registry.stopped {
            CHANGED.wait(&mut registry);
        }
        registry.stopped = true;
        STOP.store(true, Ordering::SeqCst);
        while registry
            .records
            .iter()
            .any(|record| record.running.load(Ordering::SeqCst))
        {
            CHANGED.wait(&mut registry);
        }
        let world = World::snapshot(&registry);
        drop(registry);

        /// Ends the stop even if `f` unwinds.
        struct Release(u8);
        impl Drop for Release {
            fn drop(&mut self) {
                MODE.with(|mode| mode.set(self.0));
                let mut registry = REGISTRY.lock();
                registry.stopped = false;
                STOP.store(false, Ordering::SeqCst);
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
}

/// Saves the current thread's mutator state; see [`ThreadCheckpoint`].
pub fn checkpoint() -> ThreadCheckpoint {
    ThreadCheckpoint {
        mode: MODE.with(Cell::get),
        depth: DEPTH.with(Cell::get),
        low: record().map_or(0, |record| record.low.load(Ordering::Relaxed)),
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
        STOP.store(false, Ordering::SeqCst);
        CHANGED.notify_all();
    }
    let mode = MODE.with(Cell::get);
    match (mode, checkpoint.mode) {
        (RUNNING, IDLE) => make_idle(&record),
        (IDLE, RUNNING) => make_running(&record),
        _ => {}
    }
    DEPTH.with(|depth| depth.set(checkpoint.depth));
    record.active.store(checkpoint.depth > 0, Ordering::Relaxed);
    record.low.store(checkpoint.low, Ordering::Relaxed);
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
    record: Arc<Record>,
}

impl AllocationSection {
    fn begin() -> Self {
        let record = record().expect("checked by the caller");
        make_running(&record);
        Self { record }
    }
}

impl Drop for AllocationSection {
    fn drop(&mut self) {
        make_idle(&self.record);
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
