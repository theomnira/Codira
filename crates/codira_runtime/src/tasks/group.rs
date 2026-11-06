//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The runtime half of `group { .. }`: a set of tasks whose results come
//!   back, in spawn order, when the group ends; the lowest-index error wins
//!   and cancels the tasks spawned after it.
//!
//! # Lifecycle
//!
//! Compiled code calls `codira.group_begin(result_size)`, then
//! `codira.group_spawn` once per `spawn`, then `codira.group_end` (which
//! joins every task and returns how many there were), reads the results it
//! needs with `codira.group_result` / `codira.group_first_error`, and
//! finally `codira.group_free`. On a path that abandons the group (`return`,
//! `break`, `?`, an effect handler aborting) it calls `group_end` and
//! `group_free` and reads nothing.
//!
//! # Results and the collector
//!
//! A task's trampoline writes its function's result into a buffer of the
//! task's own, and returns whether that result is an `Err` (always 0 for a
//! group whose tasks do not return a `Result`). Still running Codira code,
//! the worker copies the buffer into slot `index` of the group's
//! [`RootRegion`] -- memory the collector scans conservatively -- so a
//! result that refers to heap objects (a `String`, an `AnyError`) keeps
//! them alive until the joining task has copied it onto its own stack. No
//! safepoint lies between the trampoline's return and that copy.
//!
//! A result may be any type: a task takes only scalars and there is no
//! module-level state, so everything a task's result refers to was made by
//! that task, which has finished -- handing it over is a move, not sharing.
//!
//! # Errors and cancellation (the rule)
//!
//! When task `i` returns `Err`, every task with an index above `i` is
//! cancelled: those not yet started are never started, and those running
//! see `cancelled()` become true. Tasks below `i` are not touched. The
//! group's value is the `Err` of the *lowest-index* task that returned one,
//! or `Ok` of every result, in spawn order.
//!
//! That makes the value deterministic, and equal to what running the tasks
//! one after another and stopping at the first `Err` would give, as long as
//! each task's own result is: the lowest failing index `k` is never
//! cancelled (only a failure below it could cancel it), every task below
//! `k` runs to completion, and every task that was skipped has an index
//! above some failure, so its slot is never read.
//!
//! A group's tasks take only scalars (no channels), so a task that is never
//! started cannot leave a sibling waiting for it.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use codira_memory::gc::root_region::RootRegion;

use super::{copy_args, fatal, scheduler, Job, Owner, TaskSet};
use crate::{FunctionDefinition, IntoFunctionDefinition};

/// A group task's entry point: a compiler-generated trampoline that unpacks
/// the arguments at the first pointer, calls the function, writes its
/// result to the second and returns whether it is an `Err`.
pub(super) type GroupEntry = extern "C" fn(*mut u8, *mut u8) -> usize;

pub(crate) struct GroupState {
    pub(super) set: Arc<TaskSet>,
    /// Bytes of one result.
    result_size: usize,
    /// Words of one result slot in `results`.
    stride: usize,
    /// One slot per task, in spawn order.
    results: Arc<RootRegion>,
    /// The lowest index of a task that returned an `Err`; `usize::MAX` if
    /// none did.
    first_error: AtomicUsize,
}

impl GroupState {
    /// Runs task `index`: calls its trampoline, stores its result and, if
    /// it failed, cancels the tasks after it. Called with a mutator guard
    /// held.
    pub(super) fn run_task(&self, entry: usize, args: &mut [u128], index: usize) {
        // SAFETY: `group_spawn` was given a trampoline of this signature.
        let entry: GroupEntry = unsafe { std::mem::transmute::<usize, GroupEntry>(entry) };
        let mut out = vec![0u128; self.result_size.div_ceil(16)];
        let failed = entry(args.as_mut_ptr().cast::<u8>(), out.as_mut_ptr().cast::<u8>()) != 0;
        // Still running: no collection can start before the result is in
        // the region.
        let stride = self.stride;
        let size = self.result_size;
        self.results.with(|words| {
            let slot = &mut words[index * stride..(index + 1) * stride];
            // SAFETY: `slot` has `stride * 8 >= size` bytes and `out` has
            // `ceil(size / 16) * 16 >= size`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    out.as_ptr().cast::<u8>(),
                    slot.as_mut_ptr().cast::<u8>(),
                    size,
                );
            }
        });
        if failed {
            self.first_error.fetch_min(index, Ordering::AcqRel);
            self.set.cancel_from.fetch_min(index + 1, Ordering::AcqRel);
        }
    }
}

/// # Safety
/// `group` is a live handle from `group_begin` not yet freed.
unsafe fn group_ref<'a>(group: usize) -> &'a GroupState {
    // SAFETY: guaranteed by the caller.
    unsafe { &*(group as *const GroupState) }
}

extern "C" fn group_begin(result_size: usize) -> usize {
    Arc::into_raw(Arc::new(GroupState {
        set: TaskSet::new(),
        result_size,
        stride: result_size.div_ceil(std::mem::size_of::<usize>()),
        results: RootRegion::new(),
        first_error: AtomicUsize::new(usize::MAX),
    })) as usize
}

extern "C" fn group_spawn(group: usize, entry: usize, args: usize, size: usize) {
    if entry == 0 {
        fatal("spawned a task without an entry point");
    }
    // SAFETY: the compiler only spawns inside an open group.
    let group = unsafe {
        Arc::increment_strong_count(group as *const GroupState);
        Arc::from_raw(group as *const GroupState)
    };
    let index = group.set.add();
    let stride = group.stride;
    group
        .results
        .with(|words| words.resize((index + 1) * stride, 0));
    scheduler().push(Job {
        entry,
        args: copy_args(args, size),
        owner: Owner::Group(group),
        index,
        priority: super::scheduler::current_priority(),
    });
}

/// Joins every task of the group; returns how many there were.
extern "C" fn group_end(group: usize) -> usize {
    // SAFETY: the compiler ends each group it began exactly once, before
    // freeing it.
    let group = unsafe { group_ref(group) };
    group.set.join();
    group.set.next_index.load(Ordering::Acquire)
}

/// The lowest index of a task that returned an `Err`, or the number of
/// tasks if none did. Only after `group_end`.
extern "C" fn group_first_error(group: usize) -> usize {
    // SAFETY: as for `group_end`.
    let group = unsafe { group_ref(group) };
    let count = group.set.next_index.load(Ordering::Acquire);
    group.first_error.load(Ordering::Acquire).min(count)
}

/// Copies task `index`'s result to `out`. Only after `group_end`, and only
/// for a task that ran (see the rule in the module documentation).
extern "C" fn group_result(group: usize, index: usize, out: usize) {
    // SAFETY: as for `group_end`.
    let group = unsafe { group_ref(group) };
    if index >= group.set.next_index.load(Ordering::Acquire) {
        fatal("read the result of a task the group does not have");
    }
    let (stride, size) = (group.stride, group.result_size);
    group.results.with(|words| {
        // SAFETY: `out` is a stack slot of the result type (`size` bytes).
        unsafe {
            std::ptr::copy_nonoverlapping(
                words[index * stride..].as_ptr().cast::<u8>(),
                out as *mut u8,
                size,
            );
        }
    });
}

extern "C" fn group_free(group: usize) {
    // SAFETY: the compiler frees each group once, after `group_end`.
    let group = unsafe { Arc::from_raw(group as *const GroupState) };
    if !group.set.ended.load(Ordering::Acquire) {
        fatal("a group was freed before its tasks were joined");
    }
}

pub(super) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            group_begin as extern "C" fn(usize) -> usize,
            "codira.group_begin",
        ),
        IntoFunctionDefinition::into(
            group_spawn as extern "C" fn(usize, usize, usize, usize),
            "codira.group_spawn",
        ),
        IntoFunctionDefinition::into(
            group_end as extern "C" fn(usize) -> usize,
            "codira.group_end",
        ),
        IntoFunctionDefinition::into(
            group_first_error as extern "C" fn(usize) -> usize,
            "codira.group_first_error",
        ),
        IntoFunctionDefinition::into(
            group_result as extern "C" fn(usize, usize, usize),
            "codira.group_result",
        ),
        IntoFunctionDefinition::into(group_free as extern "C" fn(usize), "codira.group_free"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{is_cancelled, CancelToken};

    fn spawn(group: usize, entry: GroupEntry, arg: i64) {
        group_spawn(
            group,
            entry as usize,
            std::ptr::from_ref(&arg) as usize,
            std::mem::size_of::<i64>(),
        );
    }

    fn result(group: usize, index: usize) -> i64 {
        let mut out = 0i64;
        group_result(group, index, std::ptr::from_mut(&mut out) as usize);
        out
    }

    extern "C" fn square(args: *mut u8, out: *mut u8) -> usize {
        // SAFETY: the tests pass an `i64` and an `i64` slot.
        unsafe {
            let x = *args.cast::<i64>();
            *out.cast::<i64>() = x * x;
        }
        0
    }

    /// Results come back in spawn order, whatever order the tasks ran in.
    #[test]
    fn results_are_in_spawn_order() {
        let group = group_begin(8);
        for i in 0..2000 {
            spawn(group, square, i);
        }
        assert_eq!(group_end(group), 2000);
        assert_eq!(group_first_error(group), 2000);
        for i in 0..2000 {
            assert_eq!(result(group, i as usize), i * i);
        }
        group_free(group);
    }

    /// Fails (returns "Err") when its argument is negative.
    extern "C" fn fail_if_negative(args: *mut u8, out: *mut u8) -> usize {
        // SAFETY: as for `square`.
        let x = unsafe { *args.cast::<i64>() };
        // SAFETY: as for `square`.
        unsafe { *out.cast::<i64>() = x };
        usize::from(x < 0)
    }

    /// The lowest-index error wins however the tasks interleave, and the
    /// tasks before it all ran.
    #[test]
    fn lowest_index_error_wins() {
        for _ in 0..50 {
            let group = group_begin(8);
            for i in 0..20i64 {
                // Tasks 7 and 13 fail.
                spawn(group, fail_if_negative, if i == 7 || i == 13 { -i } else { i });
            }
            assert_eq!(group_end(group), 20);
            assert_eq!(group_first_error(group), 7);
            assert_eq!(result(group, 7), -7);
            for i in 0..7 {
                assert_eq!(result(group, i), i as i64);
            }
            group_free(group);
        }
    }

    extern "C" fn wait_for_cancel(_args: *mut u8, out: *mut u8) -> usize {
        let start = std::time::Instant::now();
        while !is_cancelled() {
            assert!(start.elapsed().as_secs() < 10, "never cancelled");
            std::thread::yield_now();
        }
        // SAFETY: as for `square`.
        unsafe { *out.cast::<i64>() = 1 };
        0
    }

    extern "C" fn fail_now(_args: *mut u8, _out: *mut u8) -> usize {
        1
    }

    /// A failure cancels the running tasks spawned after it, not the ones
    /// before it.
    #[test]
    fn a_failure_cancels_later_running_siblings() {
        let group = group_begin(8);
        spawn(group, square, 3);
        spawn(group, fail_now, 0);
        spawn(group, wait_for_cancel, 0);
        spawn(group, wait_for_cancel, 0);
        assert_eq!(group_end(group), 4);
        assert_eq!(group_first_error(group), 1);
        assert_eq!(result(group, 0), 9);
        group_free(group);
    }

    extern "C" fn nested(args: *mut u8, out: *mut u8) -> usize {
        // SAFETY: as for `square`.
        let width = unsafe { *args.cast::<i64>() };
        let inner = group_begin(8);
        for _ in 0..width {
            spawn(inner, wait_for_cancel, 0);
        }
        group_end(inner);
        let total: i64 = (0..width as usize).map(|i| result(inner, i)).sum();
        group_free(inner);
        // SAFETY: as for `square`.
        unsafe { *out.cast::<i64>() = total };
        0
    }

    /// Cancelling the root cancels every task below it, through nested
    /// groups.
    #[test]
    fn cancellation_reaches_grandchildren() {
        let token = CancelToken::new();
        let canceller = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                token.cancel();
            })
        };
        token.run(|| {
            let group = group_begin(8);
            for _ in 0..8 {
                spawn(group, nested, 4);
            }
            assert_eq!(group_end(group), 8);
            let total: i64 = (0..8).map(|i| result(group, i)).sum();
            assert_eq!(total, 32, "every grandchild saw the cancellation");
            group_free(group);
        });
        canceller.join().unwrap();
    }
}
