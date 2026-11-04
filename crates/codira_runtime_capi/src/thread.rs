//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Registers host threads that call Codira code directly, so collections
//!   started on other threads stop them and scan their stacks.
//!
//! A C host that calls a Codira function through the pointer
//! `codira_function_fn_ptr` returns must bracket the call with
//! [`codira_thread_enter`] and [`codira_thread_leave`] on the calling thread.
//! Between the two, a collection (`codira_gc_collect`, or an allocation at
//! the heap limit, on any thread) waits for the thread to reach a safepoint
//! and treats every word of its stack as a potential reference. Without the
//! bracket, the collector does not know the thread exists and may free
//! objects only that call refers to.
//!
//! While registered, a thread must not wait for another thread that may be
//! collecting (a lock, a join): the collection waits for this thread, and
//! the two would wait for each other.

use std::cell::RefCell;

use codira_runtime::MutatorGuard;

thread_local! {
    static GUARDS: RefCell<Vec<MutatorGuard>> = const { RefCell::new(Vec::new()) };
}

/// Registers the calling thread as running Codira code, until the matching
/// [`codira_thread_leave`]. Calls nest.
#[no_mangle]
pub extern "C" fn codira_thread_enter() {
    let guard = codira_runtime::enter_codira();
    GUARDS.with(|guards| guards.borrow_mut().push(guard));
}

/// Ends the innermost [`codira_thread_enter`] of the calling thread. Returns
/// `false` (and does nothing) if the thread has no registration to end.
#[no_mangle]
pub extern "C" fn codira_thread_leave() -> bool {
    GUARDS.with(|guards| guards.borrow_mut().pop()).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_leave_nest() {
        assert!(!codira_thread_leave());
        codira_thread_enter();
        codira_thread_enter();
        assert!(codira_thread_leave());
        assert!(codira_thread_leave());
        assert!(!codira_thread_leave());
    }
}
