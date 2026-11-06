//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - A `loom` model of the stop-the-world handshake in
//!   `src/gc/safepoint.rs`: mutators that become running, touch the heap,
//!   park at a safepoint and become idle, against a collector that stops
//!   the world, touches the heap and resumes it. The heap is a
//!   `loom::cell::UnsafeCell`, so loom reports any execution in which a
//!   mutator and the collector touch it without a happens-before edge --
//!   that is, any interleaving in which the handshake lets them run at
//!   once.
//! - `the_model_matches_safepoint_rs` (a normal test) checks that the
//!   atomic operations the model copies are still in `safepoint.rs` with
//!   the same orderings, so the model cannot silently drift from the code.
//!
//! Run the model with
//! `RUSTFLAGS="--cfg loom" cargo test -p codira_memory --test safepoint_loom --release`
//! (the gate runner does). `LOOM_MAX_PREEMPTIONS` bounds the search.
//!
//! # Model boundaries
//!
//! - It is a copy of the protocol's atomics, not the code itself: the
//!   registry mutex, the condition variable and the timed re-checks are
//!   replaced by yielding spin loops, which only adds interleavings.
//! - loom treats `SeqCst` loads and stores as `AcqRel` (loom issue #180),
//!   which cannot express the Dekker handshake (a store followed by a load
//!   of another location, on both sides). The model adds a `SeqCst` fence
//!   between each `SeqCst` store and the `SeqCst` load after it, which is
//!   the store-load order the C11 total order on `SeqCst` accesses gives
//!   the real code.
//! - loom does not explore load-buffering executions, and bounds
//!   preemptions; passing is evidence for these thread counts, not proof.
//! - The conservative stack scan, the registry of threads and reentrancy
//!   (a thread collecting from inside its own stop) are not modelled.

/// The lines of `safepoint.rs` the model copies, with their orderings.
const MODELLED: &[&str] = &[
    // make_running: the Dekker half of a mutator.
    "record.running.store(true, Ordering::SeqCst);\n        if !STOP.load(Ordering::SeqCst) {",
    "record.running.store(false, Ordering::SeqCst);",
    "while STOP.load(Ordering::SeqCst) {",
    // make_idle: a release store, then a relaxed look at the stop flag.
    "record.running.store(false, Ordering::Release);\n    if STOP.load(Ordering::Relaxed) {",
    // poll: the safepoint.
    "if STOP.load(Ordering::Relaxed) && MODE.with(Cell::get) == RUNNING {",
    // stop_the_world: the collector's half, and the release. The flag is set
    // through `set_stop`, which also refreshes the poll word compiled code
    // checks (one more SeqCst load and store, after the flag's store); that
    // keeps the store-load order the model relies on.
    "fn set_stop(on: bool) {\n    STOP.store(on, Ordering::SeqCst);",
    "set_stop(true);\n        let requested_at = std::time::Instant::now();\n        while registry\n            .records\n            .iter()\n            .any(|record| record.running.load(Ordering::SeqCst))",
    "set_stop(false);",
];

#[test]
fn the_model_matches_safepoint_rs() {
    let source = include_str!("../src/gc/safepoint.rs");
    for line in MODELLED {
        assert!(
            source.contains(line),
            "safepoint.rs no longer contains\n{line}\nUpdate the loom model in tests/safepoint_loom.rs to match it."
        );
    }
}

#[cfg(loom)]
mod model {
    use loom::{
        cell::UnsafeCell,
        sync::{
            atomic::{fence, AtomicBool, Ordering},
            Arc,
        },
        thread,
    };

    /// The shared state: the stop flag, each mutator's `running` flag, and
    /// each mutator's objects, which that mutator and the collector touch
    /// (mutators run at the same time as each other, on their own objects).
    /// How often a waiting side tries again.
    const ATTEMPTS: usize = 2;

    struct World {
        stop: AtomicBool,
        running: Vec<AtomicBool>,
        heap: Vec<UnsafeCell<u64>>,
        /// Leave out the store-load order of the handshake (a negative
        /// control: the model must then find a race).
        weak: bool,
    }

    impl World {
        fn with(mutators: usize, weak: bool) -> Arc<Self> {
            Arc::new(Self {
                weak,
                stop: AtomicBool::new(false),
                running: (0..mutators).map(|_| AtomicBool::new(false)).collect(),
                heap: (0..mutators).map(|_| UnsafeCell::new(0)).collect(),
            })
        }

        /// `make_running`, trying at most `ATTEMPTS` times: returns whether
        /// the thread became running. (The real loop waits for the stop to
        /// end and tries again; every attempt is this same code, so a
        /// bounded number of attempts covers the interleavings that
        /// matter for safety, and keeps the search finite.)
        fn make_running(&self, me: usize) -> bool {
            for _ in 0..ATTEMPTS {
                self.running[me].store(true, Ordering::SeqCst);
                self.store_load_order();
                if !self.stop.load(Ordering::SeqCst) {
                    return true;
                }
                self.running[me].store(false, Ordering::SeqCst);
                // `wait_for_stop_to_end`, once.
                if self.stop.load(Ordering::SeqCst) {
                    thread::yield_now();
                }
            }
            false
        }

        /// What the C11 total order on `SeqCst` accesses gives between a
        /// `SeqCst` store and a later `SeqCst` load (see the module docs).
        fn store_load_order(&self) {
            if !self.weak {
                fence(Ordering::SeqCst);
            }
        }

        /// `make_idle`.
        fn make_idle(&self, me: usize) {
            self.running[me].store(false, Ordering::Release);
            // The real code wakes a waiting collector here; the model's
            // collector polls, as the real one also does every millisecond.
            let _ = self.stop.load(Ordering::Relaxed);
        }

        /// `poll`: park if a stop is requested. Returns whether the thread
        /// is running again.
        fn poll(&self, me: usize) -> bool {
            if self.stop.load(Ordering::Relaxed) {
                self.make_idle(me);
                self.make_running(me)
            } else {
                true
            }
        }

        /// `stop_the_world`, with the collection itself in between, trying
        /// at most `ATTEMPTS` times to find every mutator parked.
        fn collect(&self) {
            self.stop.store(true, Ordering::SeqCst);
            self.store_load_order();
            for _ in 0..ATTEMPTS {
                if !self.running.iter().any(|r| r.load(Ordering::SeqCst)) {
                    // The world is stopped: the collector owns every heap.
                    for objects in &self.heap {
                        objects.with_mut(|objects| unsafe { *objects = 0 });
                    }
                    break;
                }
                thread::yield_now();
            }
            self.stop.store(false, Ordering::SeqCst);
        }

        /// A mutator's work: allocate (touch its objects), reach a
        /// safepoint, allocate again, leave.
        fn mutate(&self, me: usize, broken: bool) {
            if !self.make_running(me) {
                return;
            }
            self.heap[me].with_mut(|objects| unsafe { *objects += 1 });
            if self.poll(me) {
                self.heap[me].with_mut(|objects| unsafe { *objects += 1 });
                self.make_idle(me);
            }
            if broken {
                // A bug the model must catch: touching the heap after
                // becoming idle.
                self.heap[me].with_mut(|objects| unsafe { *objects += 1 });
            }
        }
    }

    fn run(mutators: usize, broken: bool) {
        run_with(mutators, broken, false);
    }

    fn run_with(mutators: usize, broken: bool, weak: bool) {
        let mut builder = loom::model::Builder::new();
        // One mutator is explored exhaustively; with two, the search is
        // bounded to 3 preemptions unless LOOM_MAX_PREEMPTIONS says
        // otherwise (unbounded, it does not finish in an hour).
        if mutators > 1 && builder.preemption_bound.is_none() {
            builder.preemption_bound = Some(3);
        }
        builder.check(move || {
            let world = World::with(mutators, weak);
            let threads: Vec<_> = (0..mutators)
                .map(|me| {
                    let world = world.clone();
                    thread::spawn(move || world.mutate(me, broken))
                })
                .collect();
            world.collect();
            for thread in threads {
                thread.join().unwrap();
            }
        });
    }

    #[test]
    fn one_mutator_never_runs_during_a_collection() {
        run(1, false);
    }

    #[test]
    fn two_mutators_never_run_during_a_collection() {
        run(2, false);
    }

    /// The model is not vacuous: a mutator that touches the heap after
    /// parking is caught.
    #[test]
    #[should_panic]
    fn touching_the_heap_while_idle_is_caught() {
        run(1, true);
    }

    /// The store-load order is what makes the handshake work: with only
    /// release and acquire (what loom makes of `SeqCst` accesses), a
    /// mutator and the collector can both proceed, and the model says so.
    #[test]
    #[should_panic]
    fn a_handshake_without_store_load_order_is_caught() {
        run_with(1, false, true);
    }
}
