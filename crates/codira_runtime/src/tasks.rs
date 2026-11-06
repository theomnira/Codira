//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - The runtime half of Codira's structured concurrency: the worker pool that
//!   runs `spawn`ed tasks, the `scope`s that join them, and the bounded
//!   blocking channels they talk through. Compiled code reaches these through
//!   the dispatch table under the `codira.scope_*` / `codira.channel_*` names
//!   `codira_codegen::intrinsics::task_functions` declares.
//!
//! # Scheduling
//!
//! Tasks run on the work-stealing scheduler in `scheduler` (per-worker
//! deques, priority classes, join helping, compensation for waits); see its
//! documentation. A `group` (see `group`) is a set of tasks whose results
//! come back when it ends.
//!
//! # The task tree and cancellation
//!
//! Every `scope` and `group` is a [`TaskSet`] that records the task that
//! opened it, so the sets form a tree rooted at the host call (or at a
//! [`CancelToken`]). A task is cancelled when its set cancels it or any
//! task above it is cancelled; `cancelled()` reads that. Only a group
//! cancels on its own (the tasks after a failed one); a `scope` never does,
//! and never skips a task -- its tasks talk through channels, and skipping
//! one could leave another waiting forever. Nothing is interrupted: a
//! cancelled task stops where its code checks `cancelled()`, and every
//! `scope` and `group` on its way out is joined and freed as usual, which is
//! what releases its channels and buffers.
//!
//! In every build, starting a task of a set that has already been joined,
//! or freeing a group before joining it, stops the process with a message;
//! debug builds also check, when each task ends, that its set was not
//! joined while it ran ("a task outlived its scope").
//!
//! # Scopes and channels
//!
//! A scope counts its unfinished tasks; `codira.scope_end` waits for the
//! count to reach zero (helping first, see the scheduler). A channel belongs to
//! the scope that created it and is freed with it, after every task of the
//! scope has finished -- the compiler guarantees no code can name the channel
//! after that (see `codira_hir::expr::validator::tasks`).
//!
//! Elements are scalars of at most 16 bytes, copied in and out by value;
//! tasks share nothing else. A channel whose senders and receivers do not
//! match up blocks forever, as in any language with blocking channels: a
//! `scope` waiting for such a task never ends.
//!
//! # `select`
//!
//! `codira.channel_select` waits on several channel operations at once and
//! performs exactly one. It locks every channel involved, in address order
//! (so two selects over the same channels cannot deadlock each other), and
//! collects the operations that can proceed. If there are any it performs
//! one chosen uniformly at random -- so a select that keeps finding several
//! ready arms serves each of them, and none starves. If there are none it
//! returns "default" when the select has a `default` arm; otherwise it
//! registers one waiter on every channel and sleeps. Every change to a
//! channel's queue (a send or receive, plain or by a select) wakes the
//! select waiters registered on it, so the select sleeps until something it
//! waits on changes -- it never polls. Woken, it relocks the channels,
//! removes its registration from all of them (the ones that did not wake it
//! included) and tries again; another task may have taken the value first,
//! in which case it registers and sleeps again. A sleeping select counts as
//! blocked for the scheduler, exactly like a blocked send or receive.
//!
//! # Collections
//!
//! A task runs Codira code, so it holds a mutator guard
//! (`codira_memory::gc::safepoint`) for as long as its entry runs: a
//! collection stops it at its next allocation and scans its stack. Every
//! wait here -- a channel operation, a `select`, a scope join -- runs inside a
//! blocking region, so a waiting task is already parked and never holds up a
//! collection (nor can a collection deadlock with it). A send or receive
//! first tries a few times without waiting, as a running thread; a channel's
//! lock is only ever held for a few instructions -- never across a wait, an
//! allocation or a safepoint -- so a running thread that takes it delays a
//! collection by no more than that. Values received into Codira stack slots
//! are written by the running thread, never inside a blocking region, so
//! nothing writes a stack a collector may be scanning.
//!
//! # Sessions
//!
//! `codira.session_open` makes the two endpoints of a session: two channels
//! of capacity one, one each way, and two endpoint records naming them
//! crosswise. Everything belongs to the scope, like a channel. A session
//! operation passes its payload's size, because one session carries
//! different types at different steps; the compiler has checked that both
//! sides agree on every step (`codira_hir::expr::session`).
mod group;
mod scheduler;

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use codira_memory::gc::safepoint::blocking;
use parking_lot::{Condvar, Mutex};
use scheduler::scheduler;
pub use scheduler::{blocking_io, stats, with_priority, Priority, SchedulerStats};

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// The largest channel element, in bytes: an `i128`.
const MAX_ELEMENT_SIZE: usize = 16;

/// A `scope` task's entry point: a compiler-generated trampoline that
/// unpacks its arguments and calls the spawned function.
type TaskEntry = extern "C" fn(*mut u8);

/// A queued task.
pub(crate) struct Job {
    /// The trampoline: a [`TaskEntry`] for a scope's task, a
    /// [`group::GroupEntry`] for a group's.
    entry: usize,
    /// The task's argument bytes, 16-byte aligned like any scalar needs.
    args: Box<[u128]>,
    owner: Owner,
    /// The task's position among its set's tasks, in spawn order.
    index: usize,
    priority: Priority,
}

/// What a task belongs to. Holding it keeps the set's channels (or result
/// storage) alive while the task runs.
enum Owner {
    Scope(Arc<ScopeState>),
    Group(Arc<group::GroupState>),
}

impl Job {
    fn set(&self) -> &Arc<TaskSet> {
        match &self.owner {
            Owner::Scope(scope) => &scope.set,
            Owner::Group(group) => &group.set,
        }
    }

    /// Identifies the set the task belongs to (see `take_own_child`).
    fn owner_id(&self) -> usize {
        Arc::as_ptr(self.set()) as usize
    }
}

/// The tasks of one `scope` or `group`, and its place in the task tree.
///
/// # Cancellation
///
/// A task is *cancelled* when its index is at least its set's `cancel_from`,
/// or when the task that opened its set is cancelled (and so on up the
/// tree). Only a group lowers `cancel_from`: when its task `i` fails, every
/// task after it is cancelled. Cancellation is cooperative: `cancelled()`
/// reports it, and a group does not start a task it has cancelled; nothing
/// is interrupted (see `group` for why that is what makes it safe).
pub(crate) struct TaskSet {
    /// Tasks spawned and not yet finished.
    pending: AtomicUsize,
    lock: Mutex<()>,
    finished: Condvar,
    /// The task that opened the set; `None` on a thread that is not running
    /// a task (the host), unless a [`CancelToken`] is in force.
    parent: Option<Arc<TaskCtx>>,
    /// Tasks with this index or above are cancelled.
    cancel_from: AtomicUsize,
    /// The next task's index.
    next_index: AtomicUsize,
    /// The set has been joined; no task of it may run any more.
    ended: AtomicBool,
}

/// A running task: which set it belongs to, and its index there.
pub(crate) struct TaskCtx {
    set: Arc<TaskSet>,
    index: usize,
}

thread_local! {
    /// The task running on this thread, innermost (a joining worker that
    /// runs one of its children replaces it for the duration).
    static CURRENT: RefCell<Option<Arc<TaskCtx>>> = const { RefCell::new(None) };
}

impl TaskCtx {
    fn is_cancelled(&self) -> bool {
        let mut task = self;
        loop {
            if task.index >= task.set.cancel_from.load(Ordering::Acquire) {
                return true;
            }
            match &task.set.parent {
                Some(parent) => task = parent,
                None => return false,
            }
        }
    }
}

impl TaskSet {
    /// A set opened by the task running on this thread.
    fn new() -> Arc<TaskSet> {
        Arc::new(TaskSet {
            pending: AtomicUsize::new(0),
            lock: Mutex::new(()),
            finished: Condvar::new(),
            parent: CURRENT.with(|current| current.borrow().clone()),
            cancel_from: AtomicUsize::new(usize::MAX),
            next_index: AtomicUsize::new(0),
            ended: AtomicBool::new(false),
        })
    }

    /// Accounts for a new task and returns its index.
    fn add(&self) -> usize {
        if self.ended.load(Ordering::Relaxed) {
            fatal("a task was spawned into a scope that has ended");
        }
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.next_index.fetch_add(1, Ordering::Relaxed)
    }

    fn finish_one(&self) {
        if self.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            let _lock = self.lock.lock();
            self.finished.notify_all();
        }
    }

    /// Waits until every task of the set has finished, running the ones
    /// still on top of this worker's deque itself (see the scheduler's
    /// "Joining"), and marks the set ended.
    fn join(self: &Arc<Self>) {
        let id = Arc::as_ptr(self) as usize;
        while self.pending.load(Ordering::Acquire) > 0 {
            let Some(job) = scheduler().take_own_child(id) else {
                break;
            };
            run(job);
        }
        if self.pending.load(Ordering::Acquire) > 0 {
            blocking(|| {
                let mut lock = self.lock.lock();
                wait_while(&self.finished, &mut lock, |_| {
                    self.pending.load(Ordering::Acquire) > 0
                });
            });
        }
        self.ended.store(true, Ordering::Release);
    }
}

/// Where task-local allocation regions plug in; today it does nothing.
///
/// A region would hold every object a task allocates that escape analysis
/// (the compiler's, not this module's) proves does not outlive the task,
/// and be released in one step when the task ends -- the end of
/// [`run`], after the task's body returned and, for a group's task, after
/// its result was copied into the group's root region. What a region must
/// never contain: anything reachable from a task's *result* (a group hands
/// it to the joining task) or from a channel (only scalars cross them
/// today). Until the compiler can prove that, no task allocates in a
/// region, so this stays empty and costs nothing.
struct TaskRegion;

impl TaskRegion {
    /// Called as a task's body starts, with a mutator guard held.
    #[inline(always)]
    fn begin() -> TaskRegion {
        TaskRegion
    }

    /// Called once the task's body (and, in a group, the copy of its
    /// result) is done, still with the mutator guard held.
    #[inline(always)]
    fn end(self) {}
}

/// Runs `job` on the current thread, as the current task.
fn run(job: Job) {
    let set = Arc::clone(job.set());
    if set.ended.load(Ordering::Acquire) {
        // The compiler joins every set before leaving it; a task that
        // starts after that is a runtime bug, and running it could touch
        // freed channels.
        fatal("a task outlived its scope");
    }
    if job.index >= set.cancel_from.load(Ordering::Acquire) {
        // A group cancelled it before it started: its result is never read
        // (see `group`).
        scheduler::note_skipped();
    } else {
        let task = Arc::new(TaskCtx {
            set: Arc::clone(&set),
            index: job.index,
        });
        let previous = CURRENT.with(|current| current.replace(Some(task)));
        let previous_priority = scheduler::set_current_priority(job.priority);
        {
            let _guard = codira_memory::gc::safepoint::enter();
            let region = TaskRegion::begin();
            let mut args = job.args;
            match &job.owner {
                Owner::Scope(_) => {
                    // SAFETY: `scope_spawn` was given a trampoline of this
                    // signature.
                    let entry: TaskEntry =
                        unsafe { std::mem::transmute::<usize, TaskEntry>(job.entry) };
                    entry(args.as_mut_ptr().cast::<u8>());
                }
                Owner::Group(group) => group.run_task(job.entry, &mut args, job.index),
            }
            region.end();
        }
        scheduler::set_current_priority(previous_priority);
        CURRENT.with(|current| *current.borrow_mut() = previous);
        #[cfg(debug_assertions)]
        if set.ended.load(Ordering::Acquire) {
            fatal("a task outlived its scope");
        }
    }
    set.finish_one();
}

/// Whether the task running on this thread has been cancelled (see
/// [`TaskSet`]). False outside any task, unless a [`CancelToken`] in force
/// on the thread was cancelled.
pub fn is_cancelled() -> bool {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|task| task.is_cancelled())
    })
}

/// Lets the host cancel the tasks a call into Codira code spawns: inside
/// [`CancelToken::run`], every `scope` and `group` opened on this thread
/// hangs below the token in the task tree, so after
/// [`CancelToken::cancel`] (from any thread) `cancelled()` is true in every
/// task below it.
#[derive(Clone)]
pub struct CancelToken {
    task: Arc<TaskCtx>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    /// A token below the task running on this thread (if any): cancelling
    /// that task also cancels the token's tasks.
    pub fn new() -> CancelToken {
        let set = Arc::new(TaskSet {
            pending: AtomicUsize::new(0),
            lock: Mutex::new(()),
            finished: Condvar::new(),
            parent: CURRENT.with(|current| current.borrow().clone()),
            cancel_from: AtomicUsize::new(usize::MAX),
            next_index: AtomicUsize::new(1),
            ended: AtomicBool::new(false),
        });
        CancelToken {
            task: Arc::new(TaskCtx { set, index: 0 }),
        }
    }

    /// Cancels every task below the token.
    pub fn cancel(&self) {
        self.task.set.cancel_from.store(0, Ordering::Release);
    }

    /// Whether the token, or a task above it, has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.task.is_cancelled()
    }

    /// Runs `f` with the token in force on this thread.
    pub fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<Arc<TaskCtx>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|current| *current.borrow_mut() = self.0.take());
            }
        }
        let _restore =
            Restore(CURRENT.with(|current| current.replace(Some(Arc::clone(&self.task)))));
        f()
    }
}

/// Waits on `condvar` while `blocked` holds, letting the scheduler
/// compensate. Called inside a blocking region.
fn wait_while<T>(
    condvar: &Condvar,
    guard: &mut parking_lot::MutexGuard<'_, T>,
    mut blocked: impl FnMut(&T) -> bool,
) {
    while blocked(guard) {
        scheduler().block_begin();
        condvar.wait(guard);
        scheduler().block_end();
    }
}

/// A runtime error in compiled code's use of tasks: there is no caller to
/// return it to, so the process stops with a message.
fn fatal(message: &str) -> ! {
    eprintln!("codira: {message}");
    std::process::abort()
}

struct ScopeState {
    set: Arc<TaskSet>,
    /// The channels created in the scope, freed with it.
    /// Boxed so each channel keeps its address -- compiled code holds it as
    /// the channel's handle -- while the vector grows.
    #[allow(clippy::vec_box)]
    channels: Mutex<Vec<Box<Channel>>>,
    /// The session endpoints created in the scope, boxed for the same reason.
    #[allow(clippy::vec_box)]
    endpoints: Mutex<Vec<Box<Endpoint>>>,
}

/// One endpoint of a session: the channel it sends on and the one it
/// receives from (the other endpoint's, crosswise).
struct Endpoint {
    outgoing: usize,
    incoming: usize,
}

struct Channel {
    state: Mutex<ChannelState>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
    element_size: usize,
}

struct ChannelState {
    queue: VecDeque<u128>,
    /// The `select`s sleeping until this channel changes.
    selects: Vec<Arc<SelectWaiter>>,
}

impl ChannelState {
    /// Wakes every `select` waiting on the channel; called on each change
    /// to the queue, with the channel locked.
    fn wake_selects(&self) {
        for waiter in &self.selects {
            *waiter.woken.lock() = true;
            waiter.wake.notify_one();
        }
    }
}

/// One sleeping `select`, registered on every channel it waits on.
struct SelectWaiter {
    woken: Mutex<bool>,
    wake: Condvar,
}

impl Channel {
    /// Copies the element at `value` into the queue, which has room.
    ///
    /// # Safety
    /// `value` points to `element_size` readable bytes.
    unsafe fn push(&self, state: &mut ChannelState, value: usize) {
        let mut element = 0u128;
        // SAFETY: guaranteed by the caller; `element` is 16 bytes and
        // `element_size` at most that.
        unsafe {
            std::ptr::copy_nonoverlapping(
                value as *const u8,
                std::ptr::from_mut(&mut element).cast::<u8>(),
                self.element_size,
            );
        }
        state.queue.push_back(element);
        self.not_empty.notify_one();
        state.wake_selects();
    }

    /// Moves the first element of the queue, which is not empty, to `out`.
    ///
    /// # Safety
    /// `out` points to `element_size` writable bytes.
    unsafe fn pop(&self, state: &mut ChannelState, out: usize) {
        let element = state
            .queue
            .pop_front()
            .expect("the caller checked that the queue is not empty");
        self.not_full.notify_one();
        state.wake_selects();
        // SAFETY: guaranteed by the caller.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&element).cast::<u8>(),
                out as *mut u8,
                self.element_size,
            );
        }
    }
}

/// # Safety
/// `scope` is a live handle returned by `scope_begin` and not yet ended.
unsafe fn scope_ref<'a>(scope: usize) -> &'a ScopeState {
    // SAFETY: guaranteed by the caller.
    unsafe { &*(scope as *const ScopeState) }
}

extern "C" fn scope_begin() -> usize {
    Arc::into_raw(Arc::new(ScopeState {
        set: TaskSet::new(),
        channels: Mutex::new(Vec::new()),
        endpoints: Mutex::new(Vec::new()),
    })) as usize
}

/// Copies a task's argument bytes.
fn copy_args(args: usize, size: usize) -> Box<[u128]> {
    let mut copy = vec![0u128; size.div_ceil(16)].into_boxed_slice();
    if size > 0 {
        // SAFETY: the compiler passes the address and size of the stack
        // struct holding the task's arguments, alive for this call.
        unsafe {
            std::ptr::copy_nonoverlapping(args as *const u8, copy.as_mut_ptr().cast::<u8>(), size);
        }
    }
    copy
}

extern "C" fn scope_spawn(scope: usize, entry: usize, args: usize, size: usize) {
    if entry == 0 {
        fatal("spawned a task without an entry point");
    }
    // SAFETY: `scope` came from `scope_begin` and its scope has not ended:
    // the compiler only spawns inside an open scope.
    let scope = unsafe {
        Arc::increment_strong_count(scope as *const ScopeState);
        Arc::from_raw(scope as *const ScopeState)
    };
    let index = scope.set.add();
    scheduler().push(Job {
        entry,
        args: copy_args(args, size),
        owner: Owner::Scope(scope),
        index,
        priority: scheduler::current_priority(),
    });
}

extern "C" fn scope_end(scope: usize) {
    // SAFETY: the compiler ends each scope it began exactly once.
    let scope = unsafe { Arc::from_raw(scope as *const ScopeState) };
    scope.set.join();
    // Every task has finished, so nothing uses the scope's channels any
    // more; they are freed with the last reference to the scope.
}

/// `cancelled()`: 1 if the current task has been cancelled, 0 otherwise.
extern "C" fn task_cancelled() -> usize {
    usize::from(is_cancelled())
}

extern "C" fn channel_new(scope: usize, capacity: usize, element_size: usize) -> usize {
    if capacity == 0 {
        fatal("a channel needs a capacity of at least 1");
    }
    if element_size > MAX_ELEMENT_SIZE {
        fatal("a channel element is larger than 16 bytes");
    }
    let channel = Box::new(Channel {
        state: Mutex::new(ChannelState {
            queue: VecDeque::with_capacity(capacity.min(1024)),
            selects: Vec::new(),
        }),
        not_empty: Condvar::new(),
        not_full: Condvar::new(),
        capacity,
        element_size,
    });
    let handle = std::ptr::from_ref::<Channel>(&channel) as usize;
    // SAFETY: the compiler creates channels only inside an open scope.
    unsafe { scope_ref(scope) }.channels.lock().push(channel);
    handle
}

/// # Safety
/// `channel` is a handle from `channel_new` whose scope has not ended.
unsafe fn channel_ref<'a>(channel: usize) -> &'a Channel {
    // SAFETY: guaranteed by the caller.
    unsafe { &*(channel as *const Channel) }
}

/// Tries `attempt` until it succeeds or a short backoff is used up: spins
/// of 1, 2, 4, .. 64 iterations, then a few yields (as crossbeam's
/// `Backoff` does). A partner on another worker usually makes room, or a
/// value, within that, and waiting instead costs a sleep and a wake-up --
/// two system calls and microseconds of latency -- on each side. Returns
/// whether `attempt` succeeded. The caller is a running thread: the
/// backoff is bounded, so it delays a collection by microseconds at most.
fn with_backoff(mut attempt: impl FnMut() -> bool) -> bool {
    const SPIN_STEPS: u32 = 6;
    const YIELD_STEPS: u32 = 4;
    for step in 0..=SPIN_STEPS + YIELD_STEPS {
        if attempt() {
            return true;
        }
        if step <= SPIN_STEPS {
            for _ in 0..1u32 << step {
                std::hint::spin_loop();
            }
        } else {
            std::thread::yield_now();
        }
    }
    attempt()
}

extern "C" fn channel_send(channel: usize, value: usize) {
    // SAFETY: the compiler only names a channel inside the scope that owns it.
    let channel = unsafe { channel_ref(channel) };
    // Without waiting first: the lock is only ever held for a few
    // instructions, never across a wait or a safepoint, so a running
    // thread may take it.
    let sent = with_backoff(|| {
        let Some(mut state) = channel.state.try_lock() else {
            return false;
        };
        if state.queue.len() >= channel.capacity {
            return false;
        }
        // SAFETY: `value` is the address of a stack slot of the element
        // type (only read).
        unsafe { channel.push(&mut state, value) };
        true
    });
    if sent {
        return;
    }
    blocking(|| {
        let mut state = channel.state.lock();
        wait_while(&channel.not_full, &mut state, |state| {
            state.queue.len() >= channel.capacity
        });
        // SAFETY: `value` is the address of a stack slot of the element type
        // (only read).
        unsafe { channel.push(&mut state, value) };
    });
}

extern "C" fn channel_receive(channel: usize, out: usize) {
    // SAFETY: the compiler only names a channel inside the scope that owns it.
    let channel = unsafe { channel_ref(channel) };
    let mut element = 0u128;
    let element_at = std::ptr::from_mut(&mut element) as usize;
    // As for a send: try without waiting first.
    let received = with_backoff(|| {
        let Some(mut state) = channel.state.try_lock() else {
            return false;
        };
        if state.queue.is_empty() {
            return false;
        }
        // SAFETY: `element` has room for any element.
        unsafe { channel.pop(&mut state, element_at) };
        true
    });
    if !received {
        blocking(|| {
            let mut state = channel.state.lock();
            wait_while(&channel.not_empty, &mut state, |state| {
                state.queue.is_empty()
            });
            // SAFETY: `element` has room for any element.
            unsafe { channel.pop(&mut state, element_at) };
        });
    }
    // SAFETY: `out` is the address of a stack slot of the element type.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(&element).cast::<u8>(),
            out as *mut u8,
            channel.element_size,
        );
    }
}

/// Creates the two endpoints of a session in `scope` and writes their
/// handles to the two `usize`s at `out`.
extern "C" fn session_open(scope: usize, out: usize) {
    let element_size = MAX_ELEMENT_SIZE;
    let forth = channel_new(scope, 1, element_size);
    let back = channel_new(scope, 1, element_size);
    let first = Box::new(Endpoint {
        outgoing: forth,
        incoming: back,
    });
    let second = Box::new(Endpoint {
        outgoing: back,
        incoming: forth,
    });
    let handles = [
        std::ptr::from_ref::<Endpoint>(&first) as usize,
        std::ptr::from_ref::<Endpoint>(&second) as usize,
    ];
    // SAFETY: the compiler creates sessions only inside an open scope.
    let scope = unsafe { scope_ref(scope) };
    scope.endpoints.lock().extend([first, second]);
    // SAFETY: `out` is the address of two `usize`s on the caller's stack.
    unsafe { std::ptr::copy_nonoverlapping(handles.as_ptr(), out as *mut usize, 2) };
}

/// # Safety
/// `endpoint` is a handle from `session_open` whose scope has not ended.
unsafe fn endpoint_ref<'a>(endpoint: usize) -> &'a Endpoint {
    // SAFETY: guaranteed by the caller.
    unsafe { &*(endpoint as *const Endpoint) }
}

/// Sends the `size` bytes at `value` to the other endpoint.
extern "C" fn session_send(endpoint: usize, value: usize, size: usize) {
    if size > MAX_ELEMENT_SIZE {
        fatal("a session payload is larger than 16 bytes");
    }
    // SAFETY: the compiler only names an endpoint inside its scope.
    let endpoint = unsafe { endpoint_ref(endpoint) };
    let mut element = 0u128;
    // SAFETY: `value` is the address of a stack slot of `size` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            value as *const u8,
            std::ptr::from_mut(&mut element).cast::<u8>(),
            size,
        );
    }
    channel_send(endpoint.outgoing, std::ptr::from_ref(&element) as usize);
}

/// Receives the other endpoint's next message into the `size` bytes at
/// `out`.
extern "C" fn session_receive(endpoint: usize, out: usize, size: usize) {
    if size > MAX_ELEMENT_SIZE {
        fatal("a session payload is larger than 16 bytes");
    }
    // SAFETY: the compiler only names an endpoint inside its scope.
    let endpoint = unsafe { endpoint_ref(endpoint) };
    let mut element = 0u128;
    channel_receive(endpoint.incoming, std::ptr::from_mut(&mut element) as usize);
    // SAFETY: `out` is the address of a stack slot of `size` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(&element).cast::<u8>(),
            out as *mut u8,
            size,
        );
    }
}

/// One operation of a `select`, as compiled code lays it out: three
/// `usize`s.
#[repr(C)]
#[derive(Clone, Copy)]
struct SelectCase {
    channel: usize,
    /// [`SELECT_RECEIVE`] or [`SELECT_SEND`].
    kind: usize,
    /// For a send, the address of the value; for a receive, of the slot
    /// the value is received into.
    data: usize,
}

const SELECT_RECEIVE: usize = 0;
const SELECT_SEND: usize = 1;

/// A uniformly distributed number below `n` (which is not zero), from a
/// per-thread splitmix64 stream. Fairness needs no more than that.
fn random_below(n: usize) -> usize {
    static SEEDS: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            SEEDS.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed) ^ u64::from(time)
        });
    }
    let x = STATE.with(|state| {
        let next = state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        state.set(next);
        let mut z = next;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    });
    // Multiply-shift maps 64 random bits onto `0..n`, with a bias below
    // `n / 2^64`.
    let scaled = (u128::from(x) * n as u128) >> 64;
    usize::try_from(scaled).expect("below n, so it fits")
}

/// `select`: performs one of the `count` operations described at `cases`,
/// waiting until one can proceed, and returns its index -- or, when
/// `has_default` is set and none can proceed right now, returns `count`
/// without waiting.
extern "C" fn channel_select(cases: usize, count: usize, has_default: usize) -> usize {
    let cases: &[SelectCase] = if count == 0 {
        &[]
    } else {
        // SAFETY: the compiler passes the address of an array of `count`
        // cases that lives for the duration of the call.
        unsafe { std::slice::from_raw_parts(cases as *const SelectCase, count) }
    };
    // SAFETY: the compiler only names a channel inside the scope that owns
    // it, and the select runs inside that scope.
    let channel_of = |case: &SelectCase| unsafe { channel_ref(case.channel) };
    // The distinct channels, in the order they are locked.
    let mut channels: Vec<&Channel> = cases.iter().map(channel_of).collect();
    channels.sort_by_key(|channel| std::ptr::from_ref::<Channel>(channel) as usize);
    channels.dedup_by_key(|channel| std::ptr::from_ref::<Channel>(channel) as usize);
    let lock_index = |channel: &Channel| {
        channels
            .iter()
            .position(|c| std::ptr::eq(*c, channel))
            .expect("every case's channel is in the list")
    };

    if channels.is_empty() && has_default == 0 {
        fatal("a `select` with no arms waits forever");
    }
    let waiter = Arc::new(SelectWaiter {
        woken: Mutex::new(false),
        wake: Condvar::new(),
    });
    // A received value lands here first and is copied to its Codira stack
    // slot once the thread runs again.
    let mut received = 0u128;
    let received_at = std::ptr::from_mut(&mut received) as usize;
    let chosen = blocking(|| loop {
        let mut states: Vec<_> = channels.iter().map(|c| c.state.lock()).collect();
        // After a sleep, cancel the registration on every channel, including
        // the ones that did not wake this select. (Nothing to remove the
        // first time round.)
        for state in &mut states {
            state.selects.retain(|w| !Arc::ptr_eq(w, &waiter));
        }
        let ready: Vec<usize> = (0..count)
            .filter(|&i| {
                let channel = channel_of(&cases[i]);
                let state = &states[lock_index(channel)];
                if cases[i].kind == SELECT_SEND {
                    state.queue.len() < channel.capacity
                } else {
                    !state.queue.is_empty()
                }
            })
            .collect();
        if !ready.is_empty() {
            let chosen = ready[random_below(ready.len())];
            let case = cases[chosen];
            let channel = channel_of(&case);
            let state = &mut states[lock_index(channel)];
            // SAFETY: `data` is the address of a stack slot of the element
            // type, as the compiler lays a case out.
            unsafe {
                match case.kind {
                    SELECT_SEND => channel.push(state, case.data),
                    SELECT_RECEIVE => channel.pop(state, received_at),
                    _ => fatal("a `select` case of an unknown kind"),
                }
            }
            return chosen;
        }
        if has_default != 0 {
            return count;
        }
        // Nothing is ready: sleep until one of the channels changes. The
        // flag is cleared and the waiter registered under every lock, so no
        // change after this point can be missed.
        *waiter.woken.lock() = false;
        for state in &mut states {
            state.selects.push(Arc::clone(&waiter));
        }
        drop(states);
        let mut woken = waiter.woken.lock();
        wait_while(&waiter.wake, &mut woken, |woken| !*woken);
    });
    if chosen < count && cases[chosen].kind == SELECT_RECEIVE {
        let channel = channel_of(&cases[chosen]);
        // SAFETY: `data` is the address of a stack slot of the element type.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&received).cast::<u8>(),
                cases[chosen].data as *mut u8,
                channel.element_size,
            );
        }
    }
    chosen
}

/// The task functions every runtime registers.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            scope_begin as extern "C" fn() -> usize,
            "codira.scope_begin",
        ),
        IntoFunctionDefinition::into(
            scope_spawn as extern "C" fn(usize, usize, usize, usize),
            "codira.scope_spawn",
        ),
        IntoFunctionDefinition::into(scope_end as extern "C" fn(usize), "codira.scope_end"),
        IntoFunctionDefinition::into(
            channel_new as extern "C" fn(usize, usize, usize) -> usize,
            "codira.channel_new",
        ),
        IntoFunctionDefinition::into(
            channel_send as extern "C" fn(usize, usize),
            "codira.channel_send",
        ),
        IntoFunctionDefinition::into(
            channel_receive as extern "C" fn(usize, usize),
            "codira.channel_receive",
        ),
        IntoFunctionDefinition::into(
            channel_select as extern "C" fn(usize, usize, usize) -> usize,
            "codira.channel_select",
        ),
        IntoFunctionDefinition::into(
            session_open as extern "C" fn(usize, usize),
            "codira.session_open",
        ),
        IntoFunctionDefinition::into(
            session_send as extern "C" fn(usize, usize, usize),
            "codira.session_send",
        ),
        IntoFunctionDefinition::into(
            session_receive as extern "C" fn(usize, usize, usize),
            "codira.session_receive",
        ),
        IntoFunctionDefinition::into(
            task_cancelled as extern "C" fn() -> usize,
            "codira.task_cancelled",
        ),
    ]
    .into_iter()
    .chain(group::functions())
    .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn add(args: *mut u8) {
        // SAFETY: the test passes a `usize`.
        let amount = unsafe { *args.cast::<usize>() };
        COUNTER.fetch_add(amount, Ordering::SeqCst);
    }

    /// Every task of a scope has run when `scope_end` returns.
    #[test]
    fn scope_end_joins_every_task() {
        let scope = scope_begin();
        for amount in 1..=100usize {
            scope_spawn(
                scope,
                add as TaskEntry as usize,
                std::ptr::from_ref(&amount) as usize,
                std::mem::size_of::<usize>(),
            );
        }
        scope_end(scope);
        assert_eq!(COUNTER.load(Ordering::SeqCst), 5050);
    }

    extern "C" fn producer(args: *mut u8) {
        // SAFETY: the test passes a channel handle.
        let channel = unsafe { *args.cast::<usize>() };
        for value in 0..1000i64 {
            channel_send(channel, std::ptr::from_ref(&value) as usize);
        }
    }

    /// A bounded channel delivers every element, in order, to a receiver
    /// that is slower than the sender.
    #[test]
    fn channel_delivers_in_order_through_a_small_buffer() {
        let scope = scope_begin();
        let channel = channel_new(scope, 2, std::mem::size_of::<i64>());
        scope_spawn(
            scope,
            producer as TaskEntry as usize,
            std::ptr::from_ref(&channel) as usize,
            std::mem::size_of::<usize>(),
        );
        for expected in 0..1000i64 {
            let mut value = -1i64;
            channel_receive(channel, std::ptr::from_mut(&mut value) as usize);
            assert_eq!(value, expected);
        }
        scope_end(scope);
    }

    extern "C" fn receive_then_send(args: *mut u8) {
        // SAFETY: the test passes two channel handles.
        let (from, to) = unsafe { *args.cast::<(usize, usize)>() };
        let mut value = 0i32;
        channel_receive(from, std::ptr::from_mut(&mut value) as usize);
        let doubled = value * 2;
        channel_send(to, std::ptr::from_ref(&doubled) as usize);
    }

    /// More tasks blocked waiting than there are workers: the pool starts
    /// replacements, so the task that unblocks them still runs.
    #[test]
    fn blocked_workers_are_compensated() {
        let workers = scheduler().target();
        let scope = scope_begin();
        let input = channel_new(scope, 1, 4);
        let output = channel_new(scope, 1024, 4);
        let tasks = workers * 2 + 1;
        for _ in 0..tasks {
            let channels = (input, output);
            scope_spawn(
                scope,
                receive_then_send as TaskEntry as usize,
                std::ptr::from_ref(&channels) as usize,
                std::mem::size_of::<(usize, usize)>(),
            );
        }
        let mut sum = 0;
        for value in 0..i32::try_from(tasks).unwrap() {
            channel_send(input, std::ptr::from_ref(&value) as usize);
        }
        for _ in 0..tasks {
            let mut value = 0i32;
            channel_receive(output, std::ptr::from_mut(&mut value) as usize);
            sum += value;
        }
        scope_end(scope);
        let n = i32::try_from(tasks).unwrap();
        assert_eq!(sum, n * (n - 1));
    }

    /// Runs `test` on its own thread and fails if it takes longer than ten
    /// seconds: a `select` that never wakes must fail the test, not hang
    /// the suite.
    fn with_timeout(test: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            test();
            let _ = done.send(());
        });
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the test did not finish within ten seconds");
    }

    fn receive_case(channel: usize, out: &mut i64) -> SelectCase {
        SelectCase {
            channel,
            kind: SELECT_RECEIVE,
            data: std::ptr::from_mut(out) as usize,
        }
    }

    fn send_case(channel: usize, value: &i64) -> SelectCase {
        SelectCase {
            channel,
            kind: SELECT_SEND,
            data: std::ptr::from_ref(value) as usize,
        }
    }

    fn select(cases: &[SelectCase], has_default: bool) -> usize {
        channel_select(
            cases.as_ptr() as usize,
            cases.len(),
            usize::from(has_default),
        )
    }

    fn send(channel: usize, value: i64) {
        channel_send(channel, std::ptr::from_ref(&value) as usize);
    }

    fn receive(channel: usize) -> i64 {
        let mut value = 0i64;
        channel_receive(channel, std::ptr::from_mut(&mut value) as usize);
        value
    }

    extern "C" fn send_seven(args: *mut u8) {
        // SAFETY: the test passes a channel handle.
        let channel = unsafe { *args.cast::<usize>() };
        send(channel, 7);
    }

    /// A select with nothing ready sleeps, is woken by a send from another
    /// task on one of its channels, receives that value, and leaves no
    /// registration behind on the channel that stayed quiet.
    #[test]
    fn select_sleeps_until_a_channel_is_ready() {
        with_timeout(|| {
            let scope = scope_begin();
            let quiet = channel_new(scope, 1, 8);
            let busy = channel_new(scope, 1, 8);
            scope_spawn(
                scope,
                send_seven as TaskEntry as usize,
                std::ptr::from_ref(&busy) as usize,
                std::mem::size_of::<usize>(),
            );
            let (mut a, mut b) = (0i64, 0i64);
            let chosen = select(
                &[receive_case(quiet, &mut a), receive_case(busy, &mut b)],
                false,
            );
            assert_eq!((chosen, b), (1, 7));
            // SAFETY: the scope, and with it the channel, is still open.
            let leftover = unsafe { channel_ref(quiet) }.state.lock().selects.len();
            assert_eq!(leftover, 0, "the cancelled registration was removed");
            scope_end(scope);
        });
    }

    /// `default` is taken exactly when no operation can proceed.
    #[test]
    fn select_default_only_when_nothing_is_ready() {
        with_timeout(|| {
            let scope = scope_begin();
            let channel = channel_new(scope, 1, 8);
            let mut out = 0i64;
            assert_eq!(select(&[receive_case(channel, &mut out)], true), 1);
            send(channel, 5);
            assert_eq!(select(&[receive_case(channel, &mut out)], true), 0);
            assert_eq!(out, 5);
            // A send arm is ready while there is room, then not.
            let value = 9i64;
            assert_eq!(select(&[send_case(channel, &value)], true), 0);
            assert_eq!(select(&[send_case(channel, &value)], true), 1);
            assert_eq!(receive(channel), 9);
            scope_end(scope);
        });
    }

    /// Two operations that are always ready are chosen about equally often:
    /// the choice is random, not first-listed-wins.
    #[test]
    fn select_chooses_fairly_among_ready_operations() {
        with_timeout(|| {
            let scope = scope_begin();
            let first = channel_new(scope, 1, 8);
            let second = channel_new(scope, 1, 8);
            send(first, 1);
            send(second, 2);
            let mut counts = [0usize; 2];
            for _ in 0..10_000 {
                let (mut a, mut b) = (0i64, 0i64);
                let chosen = select(
                    &[receive_case(first, &mut a), receive_case(second, &mut b)],
                    false,
                );
                counts[chosen] += 1;
                // Refill whichever one was taken, so both stay ready.
                if chosen == 0 {
                    send(first, a);
                } else {
                    send(second, b);
                }
            }
            assert!(
                counts.iter().all(|&n| n > 4_000),
                "unfair choice: {counts:?}"
            );
            scope_end(scope);
        });
    }

    extern "C" fn select_then_forward(args: *mut u8) {
        // SAFETY: the test passes three channel handles.
        let (left, right, results) = unsafe { *args.cast::<(usize, usize, usize)>() };
        let (mut a, mut b) = (0i64, 0i64);
        let value = match select(
            &[receive_case(left, &mut a), receive_case(right, &mut b)],
            false,
        ) {
            0 => a,
            _ => b,
        };
        send(results, value);
    }

    extern "C" fn session_peer(args: *mut u8) {
        // SAFETY: the test passes an endpoint handle.
        let endpoint = unsafe { *args.cast::<usize>() };
        let mut request = 0i64;
        session_receive(endpoint, std::ptr::from_mut(&mut request) as usize, 8);
        let reply = request > 10;
        session_send(endpoint, std::ptr::from_ref(&reply) as usize, 1);
        let small = 7i8;
        session_send(endpoint, std::ptr::from_ref(&small) as usize, 1);
    }

    /// The two endpoints of a session talk both ways, with payloads of
    /// different sizes at different steps.
    #[test]
    fn session_endpoints_talk_both_ways() {
        with_timeout(|| {
            let scope = scope_begin();
            let mut ends = [0usize; 2];
            session_open(scope, ends.as_mut_ptr() as usize);
            scope_spawn(
                scope,
                session_peer as TaskEntry as usize,
                std::ptr::from_ref(&ends[1]) as usize,
                std::mem::size_of::<usize>(),
            );
            let request = 42i64;
            session_send(ends[0], std::ptr::from_ref(&request) as usize, 8);
            let mut reply = false;
            session_receive(ends[0], std::ptr::from_mut(&mut reply) as usize, 1);
            let mut small = 0i8;
            session_receive(ends[0], std::ptr::from_mut(&mut small) as usize, 1);
            assert!(reply);
            assert_eq!(small, 7);
            scope_end(scope);
        });
    }

    static ORDER: Mutex<Vec<u8>> = parking_lot::const_mutex(Vec::new());

    extern "C" fn record_class(args: *mut u8) {
        // SAFETY: the test passes a `u8`.
        let class = unsafe { *args };
        ORDER.lock().push(class);
        // Long enough that the workers keep finding all three classes
        // queued while the first starts are recorded.
        std::thread::sleep(std::time::Duration::from_micros(50));
    }

    /// While every class has work queued, starts follow the 4 : 2 : 1
    /// weighted round robin, and background work is not starved.
    #[test]
    fn priority_classes_share_starts_four_two_one() {
        with_timeout(|| {
            ORDER.lock().clear();
            let per_class = 3000usize;
            let scope = scope_begin();
            for i in 0..per_class * 3 {
                let class = (i % 3) as u8;
                let priority =
                    [Priority::High, Priority::Normal, Priority::Background][class as usize];
                with_priority(priority, || {
                    scope_spawn(
                        scope,
                        record_class as TaskEntry as usize,
                        std::ptr::from_ref(&class) as usize,
                        1,
                    );
                });
            }
            scope_end(scope);
            let order = ORDER.lock().clone();
            assert_eq!(order.len(), per_class * 3);
            // The first 1400 starts: the high class runs out after 3000, so
            // all three classes are still queued throughout.
            let window = &order[..1400];
            let count =
                |c: u8| window.iter().filter(|&&x| x == c).count() as f64 / window.len() as f64;
            let (high, normal, background) = (count(0), count(1), count(2));
            eprintln!("starts: high {high:.3}, normal {normal:.3}, background {background:.3}");
            assert!(
                (high - 4.0 / 7.0).abs() < 0.08,
                "high {high:.3}, normal {normal:.3}, background {background:.3}"
            );
            assert!(
                (normal - 2.0 / 7.0).abs() < 0.08,
                "high {high:.3}, normal {normal:.3}, background {background:.3}"
            );
            assert!(
                background > 1.0 / 7.0 - 0.06,
                "background starved: {background:.3}"
            );
        });
    }

    static HELPED_SUM: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn add_to_helped_sum(args: *mut u8) {
        // SAFETY: the test passes a `usize`.
        let amount = unsafe { *args.cast::<usize>() };
        HELPED_SUM.fetch_add(amount, Ordering::SeqCst);
    }

    extern "C" fn nested_scope(args: *mut u8) {
        // SAFETY: the test passes a `usize`.
        let width = unsafe { *args.cast::<usize>() };
        let scope = scope_begin();
        for amount in 0..width {
            scope_spawn(
                scope,
                add_to_helped_sum as TaskEntry as usize,
                std::ptr::from_ref(&amount) as usize,
                std::mem::size_of::<usize>(),
            );
        }
        scope_end(scope);
    }

    /// A worker that joins runs its own children: helping shows in the
    /// counters, and the result is the same.
    #[test]
    fn joining_workers_run_their_own_children() {
        with_timeout(|| {
            let before = stats();
            let scope = scope_begin();
            let width = 64usize;
            for _ in 0..32 {
                scope_spawn(
                    scope,
                    nested_scope as TaskEntry as usize,
                    std::ptr::from_ref(&width) as usize,
                    std::mem::size_of::<usize>(),
                );
            }
            scope_end(scope);
            let after = stats();
            assert_eq!(
                HELPED_SUM.load(Ordering::SeqCst),
                32 * (width * (width - 1) / 2)
            );
            assert!(after.helped > before.helped, "{after:?}");
            assert!(after.spawned - before.spawned >= 32 * 65);
        });
    }

    /// The process-level checks: run in a child process (they abort), which
    /// runs this test binary again with `CODIRA_TASKS_PROBE` set.
    fn aborts_with(probe: &str, message: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tasks::tests::probes",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env("CODIRA_TASKS_PROBE", probe)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "the probe `{probe}` did not stop the process"
        );
        assert!(stderr.contains(message), "{stderr}");
    }

    /// Runs the probe named by `CODIRA_TASKS_PROBE`; does nothing without it.
    #[test]
    fn probes() {
        match std::env::var("CODIRA_TASKS_PROBE").as_deref() {
            Ok("spawn_after_end") => {
                let scope = scope_begin();
                // SAFETY: the scope is open.
                let set = Arc::clone(&unsafe { scope_ref(scope) }.set);
                scope_end(scope);
                set.add();
            }
            Ok("start_after_end") => {
                let scope = Arc::new(ScopeState {
                    set: TaskSet::new(),
                    channels: Mutex::new(Vec::new()),
                    endpoints: Mutex::new(Vec::new()),
                });
                scope.set.join();
                run(Job {
                    entry: add_to_helped_sum as TaskEntry as usize,
                    args: copy_args(0, 0),
                    owner: Owner::Scope(scope),
                    index: 0,
                    priority: Priority::Normal,
                });
            }
            Ok("free_unjoined_group") => group::group_free(group::group_begin(8)),
            _ => {}
        }
    }

    /// No task can be added to, or started in, a set that has been joined,
    /// and a group cannot be freed before it is joined: each stops the
    /// process with a message instead of touching freed memory.
    #[test]
    fn a_task_cannot_outlive_its_scope() {
        aborts_with(
            "spawn_after_end",
            "a task was spawned into a scope that has ended",
        );
        aborts_with("start_after_end", "a task outlived its scope");
        aborts_with(
            "free_unjoined_group",
            "a group was freed before its tasks were joined",
        );
    }

    /// More sleeping selects than workers: the pool compensates for them as
    /// for any blocked task, so all of them are eventually served.
    #[test]
    fn sleeping_selects_are_compensated() {
        with_timeout(|| {
            let workers = scheduler().target();
            let scope = scope_begin();
            let left = channel_new(scope, 1, 8);
            let right = channel_new(scope, 1, 8);
            let results = channel_new(scope, 1024, 8);
            let tasks = workers * 2 + 1;
            for _ in 0..tasks {
                let channels = (left, right, results);
                scope_spawn(
                    scope,
                    select_then_forward as TaskEntry as usize,
                    std::ptr::from_ref(&channels) as usize,
                    std::mem::size_of::<(usize, usize, usize)>(),
                );
            }
            let mut expected = 0;
            for i in 0..tasks {
                let value = i64::try_from(i).unwrap();
                send(if i % 2 == 0 { left } else { right }, value);
                expected += value;
            }
            let total: i64 = (0..tasks).map(|_| receive(results)).sum();
            assert_eq!(total, expected);
            scope_end(scope);
        });
    }
}
