//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests for `group { .. }`: results in spawn order, the
//!   lowest-index error winning, cancellation of the tasks after a failure,
//!   `cancelled()` through nested groups and scopes, every exit path joining,
//!   results that live on the GC heap surviving collections, and thousands of
//!   tasks.
//!
//! Assertions are on values, never on timing: where cancellation is tested,
//! the cancelled tasks wait for `cancelled()` and would hang (and fail the
//! test's timeout) if it never came.
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use codira_runtime::{tasks, StringRef};
use codira_test::CompileAndRunTestDriver;

const SOURCE: &str = r#"
    enum Result[T, E] {
        Ok(T)
        Err(E)
    }

    extern "C" {
        func codira_time_sleep_ns(nanos: u64);
    }

    // Waits until the task is cancelled. It sleeps between checks: a sleep
    // releases the worker (a busy loop would keep it, and with every worker
    // spinning here the task whose failure cancels them could never start).
    func wait_for_cancel() {
        while !cancelled() {
            codira_time_sleep_ns(100000u64);
        }
    }

    func square(x: i64) -> i64 {
        x * x
    }

    public func squares(n: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < n {
                spawn square(i);
                i += 1;
            }
        };
        // In spawn order: a weighted sum tells a permutation apart.
        var total = 0;
        var i = 0;
        while i < xs.len() as i64 {
            total += xs[i as usize] * (i + 1);
            i += 1;
        };
        total
    }

    public func count(n: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < n {
                spawn square(i);
                i += 1;
            }
        };
        xs.len() as i64
    }

    // Fails at `bad`; waits for cancellation after it (forever, if the
    // failure did not cancel it -- the test then times out).
    func check(i: i64, bad: i64) -> Result[i64, i64] {
        if i == bad {
            return Result.Err(i * 100)
        };
        if i > bad {
            wait_for_cancel();
            return Result.Err(-1)
        };
        Result.Ok(i)
    }

    // The Ok payloads in order, or the lowest-index Err.
    public func first_failure(n: i64, bad: i64) -> i64 {
        let r = group {
            var i = 0;
            while i < n {
                spawn check(i, bad);
                i += 1;
            }
        };
        match r {
            Result.Ok(xs) -> xs.len() as i64,
            Result.Err(e) -> -e,
        }
    }

    func maybe(i: i64, bad1: i64, bad2: i64) -> Result[i64, i64] {
        if i == bad1 || i == bad2 { Result.Err(i) } else { Result.Ok(i * 2) }
    }

    // Two failures: the lower index wins, whichever finishes first.
    public func two_failures(n: i64, bad1: i64, bad2: i64) -> i64 {
        let r = group {
            var i = 0;
            while i < n {
                spawn maybe(i, bad1, bad2);
                i += 1;
            }
        };
        match r {
            Result.Ok(xs) -> {
                var s = 0;
                for x in xs { s += x };
                s
            },
            Result.Err(e) -> -1000 - e,
        }
    }

    func leaf(depth: i64) -> i64 {
        wait_for_cancel();
        depth
    }

    func inner(width: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < width {
                spawn leaf(i);
                i += 1;
            }
        };
        var s = 0;
        for x in xs { s += x };
        s
    }

    // Every leaf waits for cancellation, which only comes from the host.
    public func cancellable_tree(width: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < width {
                spawn inner(width);
                i += 1;
            }
        };
        var s = 0;
        for x in xs { s += x };
        s
    }

    func greet(i: i64) -> String {
        let s = f"task {i}: ";
        var t = s;
        var k = 0;
        while k < 20 {
            t += "x";
            k += 1;
        };
        t
    }

    // Results on the GC heap, built while the host collects.
    public func strings(n: i64) -> i64 {
        let names = group {
            var i = 0;
            while i < n {
                spawn greet(i);
                i += 1;
            }
        };
        var total = 0;
        var i = 0;
        while i < names.len() {
            let expected = greet(i as i64);
            if names[i] == expected { total += 1 };
            i += 1;
        };
        total
    }

    public func last_string(n: i64) -> String {
        let names = group {
            var i = 0;
            while i < n {
                spawn greet(i);
                i += 1;
            }
        };
        names[names.len() - 1]
    }

    public func early_exit(flag: bool) -> i64 {
        let xs = group {
            spawn square(3);
            if flag {
                return 7
            };
            spawn square(4);
        };
        xs[0] + xs[1]
    }

    func opt(i: i64) -> Result[i64, i64] {
        if i < 0 { Result.Err(i) } else { Result.Ok(i) }
    }

    func doubled(a: i64, b: i64) -> Result[[i64], i64] {
        let xs = group {
            spawn opt(a);
            spawn opt(b);
        }?;
        Result.Ok(xs)
    }

    public func propagate(a: i64, b: i64) -> i64 {
        match doubled(a, b) {
            Result.Ok(xs) -> xs[0] + xs[1],
            Result.Err(e) -> e * 10,
        }
    }

    func sum_to(n: i64) -> i64 {
        // A task that runs its own group.
        let xs = group {
            var i = 1;
            while i <= n {
                spawn square(i);
                i += 1;
            }
        };
        var s = 0;
        for x in xs { s += x };
        s
    }

    public func nested(n: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < n {
                spawn sum_to(i);
                i += 1;
            }
        };
        var s = 0;
        for x in xs { s += x };
        s
    }

    func relay(input: Channel[i64], output: Channel[i64]) {
        output <- <-input * 2
    }

    func piped(x: i64) -> i64 {
        // A group task that runs a scope with channels.
        scope {
            let a = channel(1);
            let b = channel(1);
            spawn relay(a, b);
            a <- x;
            <-b
        }
    }

    public func group_of_scopes(n: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < n {
                spawn piped(i);
                i += 1;
            }
        };
        var s = 0;
        for x in xs { s += x };
        s
    }
"#;

fn driver() -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(SOURCE, |builder| builder).expect("Failed to build test driver")
}

/// Runs `test` on its own thread and fails if it takes longer than `secs`.
fn with_timeout(secs: u64, test: impl FnOnce() + Send + 'static) {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        test();
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(secs))
        .expect("the test did not finish in time");
}

/// Results come back in spawn order.
#[test]
fn results_are_in_spawn_order() {
    let driver = driver();
    let n = 300i64;
    let total: i64 = driver.runtime.invoke("squares", (n,)).unwrap();
    let expected: i64 = (0..n).map(|i| i * i * (i + 1)).sum();
    assert_eq!(total, expected);
}

/// Several thousand tasks in one group.
#[test]
fn thousands_of_tasks() {
    let driver = driver();
    let count: i64 = driver.runtime.invoke("count", (5000i64,)).unwrap();
    assert_eq!(count, 5000);
    let before = tasks::stats();
    let count: i64 = driver.runtime.invoke("count", (20_000i64,)).unwrap();
    assert_eq!(count, 20_000);
    let after = tasks::stats();
    assert!(after.spawned - before.spawned >= 20_000);
}

/// The lowest-index failure is the group's value, and the tasks after it
/// see `cancelled()` (they would wait forever otherwise).
#[test]
fn a_failure_cancels_the_tasks_after_it() {
    with_timeout(60, || {
        let driver = driver();
        for bad in [0i64, 1, 17, 63] {
            let value: i64 = driver.runtime.invoke("first_failure", (64i64, bad)).unwrap();
            assert_eq!(value, -(bad * 100));
        }
        // No failure: every task's Ok.
        let value: i64 = driver.runtime.invoke("first_failure", (64i64, 1000i64)).unwrap();
        assert_eq!(value, 64);
    });
}

/// With two failures the lower index wins every time, however the tasks
/// interleave.
#[test]
fn the_lowest_index_error_wins_deterministically() {
    let driver = driver();
    for _ in 0..50 {
        let value: i64 = driver.runtime.invoke("two_failures", (200i64, 150i64, 40i64)).unwrap();
        assert_eq!(value, -1040);
    }
    let value: i64 = driver.runtime.invoke("two_failures", (10i64, 100i64, 200i64)).unwrap();
    assert_eq!(value, 90);
}

/// The host cancels a call: every task of a two-level tree sees it.
#[test]
fn host_cancellation_reaches_every_task_of_the_tree() {
    with_timeout(60, || {
        let driver = driver();
        let token = tasks::CancelToken::new();
        let canceller = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                token.cancel();
            })
        };
        let width = 6i64;
        let value: i64 = token.run(|| driver.runtime.invoke("cancellable_tree", (width,)).unwrap());
        assert_eq!(value, width * (0..width).sum::<i64>());
        canceller.join().unwrap();
    });
}

/// Strings built by tasks survive collections from another thread between
/// the task's end and the join.
#[test]
fn heap_results_survive_collections() {
    with_timeout(120, || {
        let driver = driver();
        let gc = driver.runtime.gc();
        let stop = AtomicBool::new(false);
        let collections = std::thread::scope(|s| {
            let collector = s.spawn(|| {
                let mut collections = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    gc.collect();
                    collections += 1;
                    std::thread::sleep(Duration::from_micros(200));
                }
                collections
            });
            // Stops the collecting thread even when an assertion fails.
            struct SetOnDrop<'a>(&'a AtomicBool);
            impl Drop for SetOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let done = SetOnDrop(&stop);
            for _ in 0..20 {
                let matched: i64 = driver.runtime.invoke("strings", (200i64,)).unwrap();
                assert_eq!(matched, 200);
            }
            let last: StringRef<'_> = driver.runtime.invoke("last_string", (50i64,)).unwrap();
            assert_eq!(last.as_str(), "task 49: xxxxxxxxxxxxxxxxxxxx");
            drop(done);
            collector.join().unwrap()
        });
        assert!(collections > 0);
    });
}

/// `return` from inside a group's block joins the tasks spawned so far.
#[test]
fn every_exit_path_joins() {
    let driver = driver();
    let value: i64 = driver.runtime.invoke("early_exit", (true,)).unwrap();
    assert_eq!(value, 7);
    let value: i64 = driver.runtime.invoke("early_exit", (false,)).unwrap();
    assert_eq!(value, 25);
}

/// `?` on a group of `Result`s passes its error on.
#[test]
fn question_mark_on_a_group() {
    let driver = driver();
    let value: i64 = driver.runtime.invoke("propagate", (3i64, 4i64)).unwrap();
    assert_eq!(value, 7);
    let value: i64 = driver.runtime.invoke("propagate", (3i64, -4i64)).unwrap();
    assert_eq!(value, -40);
    let value: i64 = driver.runtime.invoke("propagate", (-3i64, -4i64)).unwrap();
    assert_eq!(value, -30);
}

/// Tasks run groups and scopes of their own.
#[test]
fn groups_nest_with_groups_and_scopes() {
    let driver = driver();
    let value: i64 = driver.runtime.invoke("nested", (30i64,)).unwrap();
    let expected: i64 = (0..30i64).map(|n| (1..=n).map(|i| i * i).sum::<i64>()).sum();
    assert_eq!(value, expected);
    let value: i64 = driver.runtime.invoke("group_of_scopes", (100i64,)).unwrap();
    assert_eq!(value, (0..100).map(|i| i * 2).sum::<i64>());
}
