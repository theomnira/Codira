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
//! # The pool
//!
//! One process-wide pool, started on first use, keeps up to
//! `available_parallelism()` workers *running*. A worker that blocks -- in a
//! channel operation, or waiting for a nested scope -- stops counting as
//! running, and if work is queued and no worker is idle, a replacement is
//! started. Without that compensation a correct program could deadlock: with
//! N workers all waiting to receive, the task that would send could sit in
//! the queue forever. Idle workers beyond the target exit.
//!
//! # Scopes and channels
//!
//! A scope counts its unfinished tasks; `codira.scope_end` waits for the
//! count to reach zero. A channel belongs to the scope that created it and is
//! freed with it, after every task of the scope has finished -- the compiler
//! guarantees no code can name the channel after that (see
//! `codira_hir::expr::validator::tasks`).
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
//! blocked for the pool, exactly like a blocked send or receive.
//!
//! # Collections
//!
//! A task runs Codira code, so it holds a mutator guard
//! (`codira_memory::gc::safepoint`) for as long as its entry runs: a
//! collection stops it at its next allocation and scans its stack. Every
//! wait here -- a channel operation, a `select`, a scope join -- runs inside a
//! blocking region, so a waiting task is already parked and never holds up a
//! collection (nor can a collection deadlock with it). Channel locks are
//! taken and released inside those regions, and values received into Codira
//! stack slots are written after the region ends, so nothing writes a stack
//! a collector may be scanning.
//!
//! # Sessions
//!
//! `codira.session_open` makes the two endpoints of a session: two channels
//! of capacity one, one each way, and two endpoint records naming them
//! crosswise. Everything belongs to the scope, like a channel. A session
//! operation passes its payload's size, because one session carries
//! different types at different steps; the compiler has checked that both
//! sides agree on every step (`codira_hir::expr::session`).
use std::{
    cell::Cell,
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
};

use parking_lot::{Condvar, Mutex};

use codira_memory::gc::safepoint::blocking;

use crate::{FunctionDefinition, IntoFunctionDefinition};

/// The largest channel element, in bytes: an `i128`.
const MAX_ELEMENT_SIZE: usize = 16;

/// Stack size of a worker thread. Codira code recurses on the native stack.
const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

/// A task's entry point: a compiler-generated trampoline that unpacks its
/// arguments and calls the spawned function.
type TaskEntry = extern "C" fn(*mut u8);

struct Job {
    entry: TaskEntry,
    /// The task's argument bytes, 16-byte aligned like any scalar needs.
    args: Box<[u128]>,
    scope: Arc<ScopeState>,
}

struct PoolState {
    queue: VecDeque<Job>,
    /// Workers waiting for work.
    idle: usize,
    /// Workers running a job and not blocked.
    running: usize,
}

struct Pool {
    state: Mutex<PoolState>,
    work_ready: Condvar,
    /// How many workers should be running at once.
    target: usize,
}

thread_local! {
    /// Whether this thread is one of the pool's workers.
    static IS_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        state: Mutex::new(PoolState {
            queue: VecDeque::new(),
            idle: 0,
            running: 0,
        }),
        work_ready: Condvar::new(),
        target: std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
    })
}

impl Pool {
    /// Starts a worker, counted as running from the start.
    fn start_worker(&'static self, state: &mut PoolState) {
        state.running += 1;
        std::thread::Builder::new()
            .name("codira-task".to_string())
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || self.work())
            .unwrap_or_else(|e| fatal(&format!("could not start a task worker: {e}")));
    }

    fn push(&'static self, job: Job) {
        let mut state = self.state.lock();
        state.queue.push_back(job);
        if state.idle > 0 {
            self.work_ready.notify_one();
        } else if state.running < self.target {
            self.start_worker(&mut state);
        }
    }

    fn work(&'static self) {
        IS_WORKER.with(|w| w.set(true));
        let mut state = self.state.lock();
        loop {
            if let Some(job) = state.queue.pop_front() {
                drop(state);
                run(job);
                state = self.state.lock();
                continue;
            }
            state.running -= 1;
            if state.idle >= self.target {
                return;
            }
            state.idle += 1;
            self.work_ready.wait(&mut state);
            state.idle -= 1;
            state.running += 1;
        }
    }

    /// The current thread is about to wait. A worker stops counting as
    /// running, and is replaced if work would otherwise sit in the queue.
    fn block_begin(&'static self) {
        if !IS_WORKER.with(std::cell::Cell::get) {
            return;
        }
        let mut state = self.state.lock();
        state.running -= 1;
        if !state.queue.is_empty() {
            if state.idle > 0 {
                self.work_ready.notify_one();
            } else if state.running < self.target {
                self.start_worker(&mut state);
            }
        }
    }

    /// The current thread has stopped waiting.
    fn block_end(&'static self) {
        if IS_WORKER.with(std::cell::Cell::get) {
            self.state.lock().running += 1;
        }
    }
}

fn run(mut job: Job) {
    {
        let _guard = codira_memory::gc::safepoint::enter();
        (job.entry)(job.args.as_mut_ptr().cast::<u8>());
    }
    let mut pending = job.scope.pending.lock();
    *pending -= 1;
    if *pending == 0 {
        job.scope.finished.notify_all();
    }
}

/// Waits on `condvar` while `blocked` holds, letting the pool compensate.
fn wait_while<T>(
    condvar: &Condvar,
    guard: &mut parking_lot::MutexGuard<'_, T>,
    mut blocked: impl FnMut(&T) -> bool,
) {
    while blocked(guard) {
        pool().block_begin();
        condvar.wait(guard);
        pool().block_end();
    }
}

/// A runtime error in compiled code's use of tasks: there is no caller to
/// return it to, so the process stops with a message.
fn fatal(message: &str) -> ! {
    eprintln!("codira: {message}");
    std::process::abort()
}

struct ScopeState {
    /// Tasks spawned and not yet finished.
    pending: Mutex<usize>,
    finished: Condvar,
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
        pending: Mutex::new(0),
        finished: Condvar::new(),
        channels: Mutex::new(Vec::new()),
        endpoints: Mutex::new(Vec::new()),
    })) as usize
}

extern "C" fn scope_spawn(scope: usize, entry: usize, args: usize, size: usize) {
    if entry == 0 {
        fatal("spawned a task without an entry point");
    }
    // SAFETY: the compiler passes the address of a task trampoline, which
    // has exactly this signature.
    let entry: TaskEntry = unsafe { std::mem::transmute::<usize, TaskEntry>(entry) };
    let mut copy = vec![0u128; size.div_ceil(16)].into_boxed_slice();
    if size > 0 {
        // SAFETY: the compiler passes the address and size of the stack
        // struct holding the task's arguments, alive for this call.
        unsafe {
            std::ptr::copy_nonoverlapping(args as *const u8, copy.as_mut_ptr().cast::<u8>(), size);
        }
    }
    // SAFETY: `scope` came from `scope_begin` and its scope has not ended:
    // the compiler only spawns inside an open scope.
    let scope = unsafe {
        Arc::increment_strong_count(scope as *const ScopeState);
        Arc::from_raw(scope as *const ScopeState)
    };
    *scope.pending.lock() += 1;
    pool().push(Job {
        entry,
        args: copy,
        scope,
    });
}

extern "C" fn scope_end(scope: usize) {
    // SAFETY: the compiler ends each scope it began exactly once.
    let scope = unsafe { Arc::from_raw(scope as *const ScopeState) };
    blocking(|| {
        let mut pending = scope.pending.lock();
        wait_while(&scope.finished, &mut pending, |pending| *pending > 0);
    });
    // Every task has finished, so nothing uses the scope's channels any
    // more; they are freed with the last reference to the scope.
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

extern "C" fn channel_send(channel: usize, value: usize) {
    // SAFETY: the compiler only names a channel inside the scope that owns it.
    let channel = unsafe { channel_ref(channel) };
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
    blocking(|| {
        let mut state = channel.state.lock();
        wait_while(&channel.not_empty, &mut state, |state| {
            state.queue.is_empty()
        });
        // SAFETY: `element` has room for any element.
        unsafe { channel.pop(&mut state, std::ptr::from_mut(&mut element) as usize) };
    });
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
    ]
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
        let workers = pool().target;
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

    /// More sleeping selects than workers: the pool compensates for them as
    /// for any blocked task, so all of them are eventually served.
    #[test]
    fn sleeping_selects_are_compensated() {
        with_timeout(|| {
            let workers = pool().target;
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
