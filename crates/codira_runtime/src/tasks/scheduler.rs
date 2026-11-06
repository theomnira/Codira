//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The work-stealing scheduler that runs Codira tasks: per-worker deques, a
//!   global injector per priority class, stealing, join helping, blocking
//!   compensation (the "blocking side"), adaptive worker counts and the
//!   counters [`stats`] reports.
//!
//! # Model
//!
//! A Codira task is *stackful and runs to completion on one OS thread*: it
//! is compiled code with native frames, so it cannot be suspended and moved.
//! The scheduler therefore decides which thread starts which task, and what
//! a thread does while its task waits; it never preempts.
//!
//! # Queues
//!
//! Every worker thread owns one deque per [`Priority`]
//! (`crossbeam_deque`, the Chase-Lev deque). A task spawned on a worker goes
//! to the bottom of that worker's deque for its class; the worker takes work
//! from the bottom (LIFO: the most recently spawned task, whose data is still
//! in cache), and idle workers steal from the top (FIFO: the oldest, usually
//! the largest piece of work). A task spawned on a thread that is not a
//! worker (the host, calling into Codira code) goes to the class's global
//! injector queue.
//!
//! # Priority classes and fairness
//!
//! Three classes, `High`, `Normal` (the default) and `Background`. Each
//! worker serves them by a fixed weighted round robin of seven slots,
//! `H N H B H N H`: for each task it starts, the slot's class is looked for
//! first (local deque, injector, other workers' deques), then the others in
//! priority order. So while every class has work, a worker starts high,
//! normal and background tasks in the ratio 4 : 2 : 1, and no class with
//! work waits more than seven starts on any worker -- background work is
//! slowed, never starved. A task inherits its spawner's class; the host
//! picks the class of the tasks its calls spawn with [`with_priority`]. The
//! ratio is a property of the start order only: tasks run to completion, so
//! a long background task keeps its thread until it ends.
//!
//! # Joining
//!
//! A worker that waits for a `scope` or `group` first *helps*: while the
//! task on top of its own deque belongs to the set it is joining, it runs
//! that task itself, on its own stack. It never runs anything else there --
//! an unrelated task could wait for something the joining task does after
//! the join, which would deadlock it. When the top of the deque is not one
//! of its children (they have all been taken, or were stolen) it blocks.
//!
//! # Blocking and compensation
//!
//! A worker whose task waits -- on a channel, a `select`, a join it cannot
//! help with, a sleep, file I/O ([`blocking_io`]) -- leaves the cooperative
//! set for the duration: it stops counting as *active*, and if tasks are
//! queued and fewer than `target` workers are active, a sleeping worker is
//! woken or a new one started. Waiting work therefore never occupies the
//! cooperative workers, and a correct program whose tasks wait for each
//! other cannot deadlock for want of a thread (the task that would release
//! them always gets one). This is Go's hand-off of a processor on a
//! blocking system call, applied to every wait. A worker that comes back
//! from a wait finishes its task; if more than `target` workers are then
//! active, it retires instead of taking another task, so the count drifts
//! back to the target.
//!
//! # Worker count
//!
//! `target` is `std::thread::available_parallelism()` (which respects CPU
//! affinity and cgroup quotas on Linux), or `CODIRA_TASK_WORKERS` when that
//! is set to a positive integer. Workers start lazily, one per spawn while
//! fewer than `target` are active and none is asleep. Workers beyond the
//! target exit as soon as they find no work; the first `target` sleep
//! after a short spin (no polling while asleep). There is no upper bound on
//! threads while tasks wait: bounding it could deadlock a correct program.
//!
//! # Collections
//!
//! A worker looking for work, spinning or asleep holds no mutator guard,
//! so it is not a thread the collector waits for. A task runs inside a
//! guard; every wait above runs inside `safepoint::blocking`, so a waiting
//! task is parked and scanned, never waited for. The scheduler's own locks
//! are taken either outside any guard or inside a blocking region, and are
//! never held across a safepoint.
//!
//! # Wake-ups
//!
//! A pusher increments `queued` and then reads `sleepers`; a worker going
//! to sleep increments `sleepers` and then reads `queued`, both `SeqCst`,
//! so at least one of them sees the other: either the pusher wakes the
//! sleeper, or the sleeper sees the task and does not sleep. The sleeper
//! holds the sleep lock from its check until it waits, and the pusher
//! notifies under it, so the notification cannot fall between the two.
//!
//! A pusher wakes nobody while some worker is *searching* (between its
//! increment and decrement of `searching`): that worker will find the task,
//! or, if it gives up, it decrements `searching` before it increments
//! `sleepers` and reads `queued`, so it sees the task then (the same
//! `SeqCst` argument). Waking one sleeper per spawned task would cost a
//! system call per task. Instead the wake-ups chain: a worker that finds a
//! task while more are queued and no one else is searching wakes one more.
use std::{
    cell::{Cell, RefCell},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use parking_lot::{Condvar, Mutex, RwLock};

use super::{fatal, Job};

/// Stack size of a worker thread. Codira code recurses on the native stack,
/// and a joining worker runs its children on its own stack.
const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

/// How many times an idle worker looks for work before it sleeps.
const SPIN_ROUNDS: usize = 64;

/// A priority class; see the module documentation for the fairness rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Priority {
    /// Latency-sensitive work: started first, four starts in seven.
    High = 0,
    /// The default: two starts in seven while every class has work.
    #[default]
    Normal = 1,
    /// Work that may wait: one start in seven while every class has work,
    /// never starved.
    Background = 2,
}

impl Priority {
    const ALL: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Background];

    fn index(self) -> usize {
        self as usize
    }
}

/// The weighted round robin: the class each worker looks for first, slot by
/// slot (4 high, 2 normal, 1 background in every seven starts).
const SCHEDULE: [Priority; 7] = [
    Priority::High,
    Priority::Normal,
    Priority::High,
    Priority::Background,
    Priority::High,
    Priority::Normal,
    Priority::High,
];

/// Counters about the scheduler since the process started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// The number of workers the scheduler keeps active.
    pub target_workers: usize,
    /// Worker threads alive now.
    pub threads: usize,
    /// The most worker threads alive at once.
    pub peak_threads: usize,
    /// Worker threads started.
    pub threads_started: usize,
    /// Tasks spawned.
    pub spawned: usize,
    /// Tasks taken from another worker's deque.
    pub stolen: usize,
    /// Tasks a joining worker ran itself.
    pub helped: usize,
    /// Waits that released a worker (channel operations, joins, sleeps,
    /// file I/O on a worker).
    pub blocked: usize,
    /// Tasks never started because a sibling with a lower index failed.
    pub skipped: usize,
    /// Tasks started per priority class: high, normal, background.
    pub started_by_priority: [usize; 3],
}

#[derive(Default)]
struct Counters {
    threads_started: AtomicUsize,
    peak_threads: AtomicUsize,
    spawned: AtomicUsize,
    stolen: AtomicUsize,
    helped: AtomicUsize,
    blocked: AtomicUsize,
    skipped: AtomicUsize,
    started: [AtomicUsize; 3],
}

/// What the sleep lock protects.
struct Sleep {
    /// Worker threads alive.
    threads: usize,
}

pub(super) struct Scheduler {
    injectors: [Injector<Job>; 3],
    /// The stealing ends of every live worker's deques, by slot.
    stealers: RwLock<Vec<Option<[Stealer<Job>; 3]>>>,
    /// Workers that are neither asleep nor waiting inside a task.
    active: AtomicUsize,
    /// Workers asleep (or about to be) on `wake`.
    sleepers: AtomicUsize,
    /// Workers looking for a task (not asleep, not running one).
    searching: AtomicUsize,
    /// Tasks pushed and not yet taken, from any queue.
    queued: AtomicUsize,
    sleep: Mutex<Sleep>,
    wake: Condvar,
    target: usize,
    counters: Counters,
}

/// The parts of a worker only its own thread touches.
struct Local {
    deques: [Worker<Job>; 3],
    slot: usize,
    /// Position in [`SCHEDULE`].
    tick: usize,
    /// State of the xorshift generator that picks the first victim.
    random: u64,
}

thread_local! {
    static LOCAL: RefCell<Option<Local>> = const { RefCell::new(None) };
    /// Whether this thread is a worker (cheaper to read than `LOCAL`).
    static IS_WORKER: Cell<bool> = const { Cell::new(false) };
    /// The priority of the task running on this thread, which the tasks it
    /// spawns inherit.
    static PRIORITY: Cell<Priority> = const { Cell::new(Priority::Normal) };
}

/// The process-wide scheduler, created on first use.
pub(super) fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(|| Scheduler {
        injectors: [Injector::new(), Injector::new(), Injector::new()],
        stealers: RwLock::new(Vec::new()),
        active: AtomicUsize::new(0),
        sleepers: AtomicUsize::new(0),
        searching: AtomicUsize::new(0),
        queued: AtomicUsize::new(0),
        sleep: Mutex::new(Sleep { threads: 0 }),
        wake: Condvar::new(),
        target: target_workers(),
        counters: Counters::default(),
    })
}

/// `CODIRA_TASK_WORKERS`, or the available parallelism.
fn target_workers() -> usize {
    std::env::var("CODIRA_TASK_WORKERS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, NonZeroUsize::get))
}

/// The priority the current thread's spawns get.
pub(super) fn current_priority() -> Priority {
    PRIORITY.with(Cell::get)
}

/// Runs `f` with the tasks it spawns (directly, or through the Codira code
/// it calls) in class `priority`. Tasks inherit their spawner's class, so
/// the whole tree below gets it.
pub fn with_priority<R>(priority: Priority, f: impl FnOnce() -> R) -> R {
    struct Restore(Priority);
    impl Drop for Restore {
        fn drop(&mut self) {
            PRIORITY.with(|p| p.set(self.0));
        }
    }
    let _restore = Restore(PRIORITY.with(|p| p.replace(priority)));
    f()
}

/// Sets the current thread's priority while a task runs on it.
pub(super) fn set_current_priority(priority: Priority) -> Priority {
    PRIORITY.with(|p| p.replace(priority))
}

/// The scheduler's counters.
pub fn stats() -> SchedulerStats {
    let s = scheduler();
    let c = &s.counters;
    SchedulerStats {
        target_workers: s.target,
        threads: s.sleep.lock().threads,
        peak_threads: c.peak_threads.load(Ordering::Relaxed),
        threads_started: c.threads_started.load(Ordering::Relaxed),
        spawned: c.spawned.load(Ordering::Relaxed),
        stolen: c.stolen.load(Ordering::Relaxed),
        helped: c.helped.load(Ordering::Relaxed),
        blocked: c.blocked.load(Ordering::Relaxed),
        skipped: c.skipped.load(Ordering::Relaxed),
        started_by_priority: [
            c.started[0].load(Ordering::Relaxed),
            c.started[1].load(Ordering::Relaxed),
            c.started[2].load(Ordering::Relaxed),
        ],
    }
}

pub(super) fn note_skipped() {
    scheduler().counters.skipped.fetch_add(1, Ordering::Relaxed);
}

impl Scheduler {
    #[cfg(test)]
    pub(super) fn target(&self) -> usize {
        self.target
    }

    /// Queues `job`: on this worker's own deque, or the injector.
    pub(super) fn push(&'static self, job: Job) {
        self.counters.spawned.fetch_add(1, Ordering::Relaxed);
        let class = job.priority.index();
        let job = LOCAL.with(|local| match &*local.borrow() {
            Some(local) => {
                local.deques[class].push(job);
                None
            }
            None => Some(job),
        });
        if let Some(job) = job {
            self.injectors[class].push(job);
        }
        self.queued.fetch_add(1, Ordering::SeqCst);
        self.wake_or_start();
    }

    /// There is work: wake a sleeping worker, or start one if fewer than
    /// `target` are active.
    fn wake_or_start(&'static self) {
        // A worker looking for work will find it: waking another costs a
        // system call per task when tasks are spawned one by one.
        if self.searching.load(Ordering::SeqCst) > 0 {
            return;
        }
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            let _sleep = self.sleep.lock();
            self.wake.notify_one();
            return;
        }
        if self.active.load(Ordering::SeqCst) < self.target {
            let mut sleep = self.sleep.lock();
            // Re-checked under the lock, so two pushers do not both start
            // the last missing worker.
            if self.active.load(Ordering::SeqCst) < self.target {
                self.start_worker(&mut sleep);
            }
        }
    }

    /// Starts a worker thread, counted as active from the start.
    fn start_worker(&'static self, sleep: &mut Sleep) {
        self.active.fetch_add(1, Ordering::SeqCst);
        sleep.threads += 1;
        self.counters
            .peak_threads
            .fetch_max(sleep.threads, Ordering::Relaxed);
        self.counters
            .threads_started
            .fetch_add(1, Ordering::Relaxed);
        std::thread::Builder::new()
            .name("codira-task".to_string())
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || self.work())
            .unwrap_or_else(|e| fatal(&format!("could not start a task worker: {e}")));
    }

    /// Registers the current thread as a worker.
    fn register(&self) {
        let deques = [Worker::new_lifo(), Worker::new_lifo(), Worker::new_lifo()];
        let stealers = [
            deques[0].stealer(),
            deques[1].stealer(),
            deques[2].stealer(),
        ];
        let mut all = self.stealers.write();
        let slot = match all.iter().position(Option::is_none) {
            Some(slot) => {
                all[slot] = Some(stealers);
                slot
            }
            None => {
                all.push(Some(stealers));
                all.len() - 1
            }
        };
        drop(all);
        IS_WORKER.with(|w| w.set(true));
        LOCAL.with(|local| {
            *local.borrow_mut() = Some(Local {
                deques,
                slot,
                tick: slot,
                random: 0x9E37_79B9_7F4A_7C15
                    ^ (slot as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9),
            });
        });
    }

    /// Unregisters the current worker, handing what is left in its deques
    /// to the injectors.
    fn unregister(&self) {
        let Some(local) = LOCAL.with(|local| local.borrow_mut().take()) else {
            return;
        };
        IS_WORKER.with(|w| w.set(false));
        self.stealers.write()[local.slot] = None;
        for (class, deque) in local.deques.iter().enumerate() {
            while let Some(job) = deque.pop() {
                self.injectors[class].push(job);
            }
        }
    }

    /// The body of a worker thread.
    fn work(&'static self) {
        self.register();
        loop {
            self.searching.fetch_add(1, Ordering::SeqCst);
            let job = self.find_job().or_else(|| self.spin_for_job());
            self.searching.fetch_sub(1, Ordering::SeqCst);
            if let Some(job) = job {
                // The last searcher found work and more is queued: wake
                // another worker to look (pushes skip the wake-up while
                // someone searches, so the wake-ups chain from here).
                if self.queued.load(Ordering::SeqCst) > 0
                    && self.searching.load(Ordering::SeqCst) == 0
                {
                    self.wake_or_start();
                }
                self.execute(job);
                // Back from a wait with the cooperative set full: retire.
                if self.active.load(Ordering::SeqCst) > self.target {
                    let mut sleep = self.sleep.lock();
                    if self.active.load(Ordering::SeqCst) > self.target {
                        self.retire(&mut sleep);
                        return;
                    }
                }
                continue;
            }
            if !self.sleep_until_work() {
                return;
            }
        }
    }

    /// Ends the current worker: called with the sleep lock held, while the
    /// worker still counts as active.
    fn retire(&self, sleep: &mut Sleep) {
        self.unregister();
        // Anything it left behind went to the injectors; make sure someone
        // will see it.
        sleep.threads -= 1;
        self.active.fetch_sub(1, Ordering::SeqCst);
        if self.queued.load(Ordering::SeqCst) > 0 {
            self.wake.notify_one();
        }
    }

    /// Looks for work a few more times, yielding now and then.
    fn spin_for_job(&self) -> Option<Job> {
        for round in 0..SPIN_ROUNDS {
            if self.queued.load(Ordering::SeqCst) > 0 {
                if let Some(job) = self.find_job() {
                    return Some(job);
                }
            }
            if round % 8 == 7 {
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
        None
    }

    /// Sleeps until there may be work. Returns false if the worker should
    /// exit instead (it is beyond the target).
    fn sleep_until_work(&'static self) -> bool {
        let mut sleep = self.sleep.lock();
        self.active.fetch_sub(1, Ordering::SeqCst);
        if sleep.threads > self.target && self.queued.load(Ordering::SeqCst) == 0 {
            sleep.threads -= 1;
            self.unregister();
            return false;
        }
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        while self.queued.load(Ordering::SeqCst) == 0 {
            self.wake.wait(&mut sleep);
        }
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Takes a task, by the weighted round robin.
    fn find_job(&self) -> Option<Job> {
        LOCAL.with(|local| {
            let mut local = local.borrow_mut();
            let local = local.as_mut().expect("only workers look for work");
            let first = SCHEDULE[local.tick % SCHEDULE.len()];
            local.tick = local.tick.wrapping_add(1);
            let order =
                std::iter::once(first).chain(Priority::ALL.into_iter().filter(|&p| p != first));
            for class in order {
                if let Some(job) = self.find_in_class(local, class.index()) {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                    self.counters.started[class.index()].fetch_add(1, Ordering::Relaxed);
                    return Some(job);
                }
            }
            None
        })
    }

    fn find_in_class(&self, local: &mut Local, class: usize) -> Option<Job> {
        if let Some(job) = local.deques[class].pop() {
            return Some(job);
        }
        loop {
            match self.injectors[class].steal_batch_and_pop(&local.deques[class]) {
                Steal::Success(job) => return Some(job),
                Steal::Retry => continue,
                Steal::Empty => break,
            }
        }
        let stealers = self.stealers.read();
        let count = stealers.len();
        if count <= 1 {
            return None;
        }
        // xorshift64: a different first victim each time.
        local.random ^= local.random << 13;
        local.random ^= local.random >> 7;
        local.random ^= local.random << 17;
        let start = (local.random % count as u64) as usize;
        loop {
            let mut retry = false;
            for offset in 0..count {
                let slot = (start + offset) % count;
                if slot == local.slot {
                    continue;
                }
                let Some(victim) = &stealers[slot] else {
                    continue;
                };
                match victim[class].steal() {
                    Steal::Success(job) => {
                        self.counters.stolen.fetch_add(1, Ordering::Relaxed);
                        return Some(job);
                    }
                    Steal::Retry => retry = true,
                    Steal::Empty => {}
                }
            }
            if !retry {
                return None;
            }
        }
    }

    fn execute(&self, job: Job) {
        super::run(job);
    }

    /// While the task on top of this worker's deque belongs to the set
    /// `owner` names, takes it so the joining task can run it. Only the
    /// current worker's deque is looked at.
    pub(super) fn take_own_child(&self, owner: usize) -> Option<Job> {
        if !IS_WORKER.with(Cell::get) {
            return None;
        }
        LOCAL.with(|local| {
            let local = local.borrow();
            let local = local.as_ref()?;
            for deque in &local.deques {
                if let Some(job) = deque.pop() {
                    if job.owner_id() == owner {
                        self.queued.fetch_sub(1, Ordering::SeqCst);
                        self.counters.helped.fetch_add(1, Ordering::Relaxed);
                        self.counters.started[job.priority.index()].fetch_add(1, Ordering::Relaxed);
                        return Some(job);
                    }
                    // Not ours: put it back where it was, and stop looking
                    // in this class (everything below it is older).
                    deque.push(job);
                }
            }
            None
        })
    }

    /// The current thread is about to wait. A worker stops counting as
    /// active, and is replaced if work would otherwise sit in a queue.
    pub(super) fn block_begin(&'static self) {
        if !IS_WORKER.with(Cell::get) {
            return;
        }
        self.counters.blocked.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_sub(1, Ordering::SeqCst);
        if self.queued.load(Ordering::SeqCst) > 0 {
            self.wake_or_start();
        }
    }

    /// The current thread has stopped waiting.
    pub(super) fn block_end(&'static self) {
        if IS_WORKER.with(Cell::get) {
            self.active.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Runs `f` -- a wait or blocking I/O that touches neither the GC heap nor
/// Codira values -- parked for the collector and, on a worker, outside the
/// cooperative set (see "Blocking and compensation").
pub fn blocking_io<R>(f: impl FnOnce() -> R) -> R {
    codira_memory::gc::safepoint::blocking(|| {
        let scheduler = scheduler();
        scheduler.block_begin();
        struct End(&'static Scheduler);
        impl Drop for End {
            fn drop(&mut self) {
                self.0.block_end();
            }
        }
        let _end = End(scheduler);
        f()
    })
}
