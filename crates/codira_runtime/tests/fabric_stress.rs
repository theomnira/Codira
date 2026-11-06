//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Stress tests for the task scheduler together with the collector: tasks
//!   that allocate, block on channels and join groups while a host thread
//!   collects back to back must neither deadlock nor lose a value; waits
//!   release their workers; the thread count drifts back to the target.
//!
//! In a test binary of its own: its collections stop the world for every
//! thread of the process, and its sleeps occupy the shared scheduler.
//! Every test has a deadline, so a deadlock fails instead of hanging.
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use codira_runtime::tasks;
use codira_test::CompileAndRunTestDriver;

const SOURCE: &str = r#"
    extern "C" {
        func codira_time_sleep_ns(nanos: u64);
    }

    // Allocates between channel operations, so a collection can stop it at
    // an allocation or find it parked in a send or receive.
    func stage(input: Channel[i64], output: Channel[i64], count: i64) {
        var i = 0;
        while i < count {
            let x = <-input;
            let garbage = [x, x + 1, x + 2];
            let s = f"{x}";
            output <- garbage[1] + s.len() as i64 - 1;
            i += 1;
        }
    }

    func source(output: Channel[i64], count: i64) {
        var i = 0;
        while i < count {
            output <- i;
            i += 1;
        }
    }

    // A pipeline of `stages` stages over channels of capacity 1, `count`
    // values through it; returns the sum that comes out.
    public func pipeline(stages: i64, count: i64) -> i64 {
        scope {
            let c0 = channel(1);
            let c1 = channel(1);
            let c2 = channel(1);
            let c3 = channel(1);
            spawn source(c0, count);
            spawn stage(c0, c1, count);
            spawn stage(c1, c2, count);
            spawn stage(c2, c3, count);
            var total = 0;
            var i = 0;
            while i < count {
                total += <-c3;
                i += 1;
            };
            total
        }
    }

    // Many independent producer/consumer pairs at once: more blocked tasks
    // than workers.
    func pair(n: i64) -> i64 {
        scope {
            let ch = channel(1);
            spawn source(ch, n);
            var total = 0;
            var i = 0;
            while i < n {
                let boxed = [<-ch];
                total += boxed[0];
                i += 1;
            };
            total
        }
    }

    public func many_pairs(pairs: i64, n: i64) -> i64 {
        let sums = group {
            var i = 0;
            while i < pairs {
                spawn pair(n);
                i += 1;
            }
        };
        var total = 0;
        for s in sums { total += s };
        total
    }

    func text(i: i64) -> String {
        var s = f"{i}";
        var k = 0;
        while k < 8 {
            s += "-";
            k += 1;
        };
        s
    }

    public func strings(n: i64) -> i64 {
        let all = group {
            var i = 0;
            while i < n {
                spawn text(i);
                i += 1;
            }
        };
        var total = 0;
        for s in all { total += s.len() as i64 };
        total
    }

    func nap(i: i64) -> i64 {
        codira_time_sleep_ns(100000000u64);
        i
    }

    public func sleepers(n: i64) -> i64 {
        let xs = group {
            var i = 0;
            while i < n {
                spawn nap(i);
                i += 1;
            }
        };
        xs.len() as i64
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
        .expect("the test did not finish in time: a deadlock?");
}

/// Collections run back to back on a host thread while tasks block on
/// channels, allocate between operations, join groups and return heap
/// values. Nothing deadlocks, and every value comes through.
#[test]
fn collections_never_deadlock_against_blocked_or_joining_tasks() {
    with_timeout(300, || {
        let driver = driver();
        let gc = driver.runtime.gc();
        let stop = AtomicBool::new(false);
        let collections = AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !stop.load(Ordering::SeqCst) {
                    gc.collect_blocking().unwrap();
                    collections.fetch_add(1, Ordering::Relaxed);
                }
            });
            struct SetOnDrop<'a>(&'a AtomicBool);
            impl Drop for SetOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _done = SetOnDrop(&stop);
            let count = 2000i64;
            // What `stage` does to a value, three times over.
            let stage = |x: i64| x + 1 + x.to_string().len() as i64 - 1;
            let expected_3: i64 = (0..count).map(|x| stage(stage(stage(x)))).sum();
            for _ in 0..5 {
                let total: i64 = driver.runtime.invoke("pipeline", (3i64, count)).unwrap();
                assert_eq!(total, expected_3);
            }
            let pairs = 64i64;
            let n = 200i64;
            for _ in 0..5 {
                let total: i64 = driver.runtime.invoke("many_pairs", (pairs, n)).unwrap();
                assert_eq!(total, pairs * n * (n - 1) / 2);
            }
            let n = 3000i64;
            let expected: i64 = (0..n).map(|i| i.to_string().len() as i64 + 8).sum();
            for _ in 0..5 {
                let total: i64 = driver.runtime.invoke("strings", (n,)).unwrap();
                assert_eq!(total, expected);
            }
        });
        assert!(
            collections.load(Ordering::Relaxed) > 10,
            "the collector ran only {} times",
            collections.load(Ordering::Relaxed)
        );
    });
}

/// A thousand tasks that each sleep 100 ms finish in far less than the
/// 25 s four workers would need one after another: a sleeping task
/// releases its worker. Then the extra workers retire, back towards the
/// target.
#[test]
fn sleeping_tasks_release_their_workers_and_extra_workers_retire() {
    with_timeout(120, || {
        let driver = driver();
        let target = tasks::stats().target_workers;
        let n = 1000i64;
        let start = Instant::now();
        let count: i64 = driver.runtime.invoke("sleepers", (n,)).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(count, n);
        let serial =
            Duration::from_millis(100) * u32::try_from(n).unwrap() / u32::try_from(target).unwrap();
        assert!(
            elapsed < serial / 4,
            "{n} sleeping tasks took {elapsed:?} (serially on {target} workers: {serial:?})"
        );
        let stats = tasks::stats();
        assert!(
            stats.peak_threads > target,
            "no worker was compensated: {stats:?}"
        );
        // Idle workers beyond the target exit.
        let deadline = Instant::now() + Duration::from_secs(30);
        while tasks::stats().threads > target && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let stats = tasks::stats();
        assert!(stats.threads <= target, "workers did not retire: {stats:?}");
    });
}
