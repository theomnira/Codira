//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Stress tests of garbage collection while several threads run Codira
//!   code: collections requested by the host from another thread, and
//!   collections forced by the heap limit while `spawn`ed tasks allocate.
//!
//! Every task keeps a few objects alive in its own frames only (nothing
//! roots them) and fills them with values derived from its seed. While it
//! runs it allocates garbage with other values, so a cell freed by mistake
//! is soon reused and overwritten, and after every allocation it checks its
//! live objects against the pattern. A task reports the number of
//! mismatches it saw; every test requires zero. A collection that misses a
//! thread's roots shows up as a mismatch (or as a crash on a reused cell).
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::time::{Duration, Instant};

use codira_runtime::Runtime;
use codira_test::{CompileAndRunTestDriver, CompileTestDriver};

const BLOCK: usize = 64 * 1024;

const SOURCE: &str = r"
    public class Leaf { a: i64, b: i64, c: i64, d: i64 }
    public class Pair { left: Leaf, right: Leaf }

    func pattern(seed: i64, k: i64) -> i64 {
        seed * 1000003 + k * 7919 + 23130
    }

    func make_leaf(seed: i64, k: i64) -> Leaf {
        Leaf {
            a: pattern(seed, k),
            b: pattern(seed, k + 1),
            c: pattern(seed, k + 2),
            d: pattern(seed, k + 3),
        }
    }

    func leaf_bad(leaf: Leaf, seed: i64, k: i64) -> i64 {
        var bad = 0;
        if leaf.a != pattern(seed, k) { bad += 1 };
        if leaf.b != pattern(seed, k + 1) { bad += 1 };
        if leaf.c != pattern(seed, k + 2) { bad += 1 };
        if leaf.d != pattern(seed, k + 3) { bad += 1 };
        bad
    }

    func churn(seed: i64, rounds: i64) -> i64 {
        // Referenced only from this frame and the objects it reaches.
        let keep = Pair { left: make_leaf(seed, 0), right: make_leaf(seed, 4) };
        let solo = make_leaf(seed, 8);
        var bad = 0;
        var i = 0;
        while i < rounds {
            // Garbage with other values: it reuses freed cells.
            let junk = Pair { left: make_leaf(0 - i - 1, 0), right: make_leaf(0 - i - 2, 4) };
            let fresh = make_leaf(seed, 100 + i);
            bad += leaf_bad(fresh, seed, 100 + i);
            bad += leaf_bad(keep.left, seed, 0);
            bad += leaf_bad(keep.right, seed, 4);
            bad += leaf_bad(solo, seed, 8);
            bad += leaf_bad(junk.left, 0 - i - 1, 0);
            i += 1;
        };
        bad
    }

    func worker(results: Channel[i64], seed: i64, rounds: i64) {
        results <- churn(seed, rounds)
    }

    public func stress(tasks: i64, rounds: i64) -> i64 {
        scope {
            let results = channel(64);
            var t = 0;
            while t < tasks {
                spawn worker(results, t + 1, rounds);
                t += 1;
            };
            // The spawning thread allocates and checks too.
            var bad = churn(1000, rounds);
            t = 0;
            while t < tasks {
                bad += <-results;
                t += 1;
            };
            bad
        }
    }
";

const TASKS: i64 = 6;

/// Sets the flag when dropped, so a collecting thread stops even when the
/// test fails (a panic inside `thread::scope` waits for it).
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// A host thread collects in a loop while tasks allocate and check their
/// objects. Before collections stopped the world, these collections freed
/// the tasks' live objects.
#[test]
fn host_collections_while_tasks_run_free_nothing_live() {
    let driver = CompileAndRunTestDriver::new(SOURCE, |builder| builder)
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let done = AtomicBool::new(false);
    let collections = AtomicUsize::new(0);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                gc.collect();
                collections.fetch_add(1, Ordering::Relaxed);
            }
        });
        let _done = SetOnDrop(&done);
        for _ in 0..4 {
            let bad: i64 = driver.runtime.invoke("stress", (TASKS, 20_000i64)).unwrap();
            assert_eq!(bad, 0, "a collection freed an object a task still used");
        }
    });
    assert!(
        collections.load(Ordering::Relaxed) > 4,
        "the host thread hardly collected"
    );
}

/// Tasks allocate far more than a small heap cap, so allocations collect
/// while every task holds live objects. Before collections stopped the
/// world, the first task to hit the cap with another task allocating
/// aborted the process.
#[test]
fn heap_limit_collections_while_tasks_run_free_nothing_live() {
    let driver = CompileAndRunTestDriver::new(SOURCE, |builder| builder.heap_limit(32 * BLOCK))
        .expect("Failed to build test driver");
    for _ in 0..4 {
        let bad: i64 = driver.runtime.invoke("stress", (TASKS, 20_000i64)).unwrap();
        assert_eq!(bad, 0, "a collection freed an object a task still used");
    }
    let gc = driver.runtime.gc();
    assert!(gc.emergency_collections() > 0, "the cap was never reached");
    assert!(gc.committed_bytes() <= 32 * BLOCK);
}

/// Both at once: a capped heap, collections from allocations on every task,
/// and from a host thread.
#[test]
fn heap_limit_and_host_collections_together() {
    let driver = CompileAndRunTestDriver::new(SOURCE, |builder| builder.heap_limit(32 * BLOCK))
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                gc.collect();
                std::thread::yield_now();
            }
        });
        let _done = SetOnDrop(&done);
        for _ in 0..3 {
            let bad: i64 = driver.runtime.invoke("stress", (TASKS, 20_000i64)).unwrap();
            assert_eq!(bad, 0, "a collection freed an object a task still used");
        }
    });
    assert!(gc.committed_bytes() <= 32 * BLOCK);
}

/// Two host threads call into Codira code at once while a third collects.
#[test]
fn concurrent_host_calls_and_collections() {
    let driver = CompileAndRunTestDriver::new(SOURCE, |builder| builder.heap_limit(32 * BLOCK))
        .expect("Failed to build test driver");
    let gc = driver.runtime.gc();
    let done = AtomicBool::new(false);
    // `Runtime` is not `Sync`; look the function up once and call it
    // through the runtime's entry guard from every thread, as a host does.
    let stress = driver
        .runtime
        .get_function_definition("stress")
        .expect("stress exists");
    let call = |tasks: i64, rounds: i64| -> i64 {
        let _guard = codira_runtime::enter_codira();
        // Safety: `stress` has this signature, and the guard registers the
        // thread for the duration of the call.
        let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(stress.fn_ptr) };
        f(tasks, rounds)
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                gc.collect();
                std::thread::yield_now();
            }
        });
        let _done = SetOnDrop(&done);
        let callers: Vec<_> = (0..2)
            .map(|_| s.spawn(|| (0..2).map(|_| call(3, 15_000)).sum::<i64>()))
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), 0);
        }
    });
}

/// A reload migrates the heap and swaps the code, so it must not happen
/// while any thread is inside Codira code: it is deferred, and the first
/// update after that thread leaves performs it -- without a new file event.
#[test]
fn a_reload_waits_until_no_thread_runs_codira_code() {
    let mut compiler = CompileTestDriver::from_file("public func value() -> i64 { 1 }");
    // Safety: we compiled the library ourselves.
    let mut runtime =
        unsafe { Runtime::builder(compiler.lib_path()).finish() }.expect("Failed to build runtime");

    let (release, released) = std::sync::mpsc::channel::<()>();
    let (entered, has_entered) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        // Like a host thread in a call through a raw function pointer,
        // waiting in a host function.
        let _guard = codira_runtime::enter_codira();
        entered.send(()).unwrap();
        codira_runtime::blocking(|| released.recv().unwrap());
    });
    has_entered.recv().unwrap();

    compiler.update_file("mod.code", "public func value() -> i64 { 2 }");
    // Long enough for the file events to arrive and be drained.
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        // Safety: we compiled the library ourselves.
        assert!(
            !unsafe { runtime.update() },
            "reloaded while a thread was inside Codira code"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let value: i64 = runtime.invoke("value", ()).unwrap();
    assert_eq!(value, 1);

    release.send(()).unwrap();
    holder.join().unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    // Safety: we compiled the library ourselves.
    while !unsafe { runtime.update() } {
        assert!(Instant::now() < until, "the deferred reload never happened");
        std::thread::sleep(Duration::from_millis(1));
    }
    let value: i64 = runtime.invoke("value", ()).unwrap();
    assert_eq!(value, 2);
}
