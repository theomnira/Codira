//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for `select`: programs that wait on several channel
//!   operations at once, compiled and run on the runtime's worker pool.
//!
//! Every assertion is on values computed from what the selects received, and
//! every test runs under a hard timeout so a select that never wakes fails
//! instead of hanging the suite.
use std::{sync::mpsc, thread, time::Duration};

use codira_test::CompileAndRunTestDriver;

const SOURCE: &str = r"
    func produce(dest: Channel[i64], from: i64, count: i64) {
        var i = 0;
        while i < count {
            dest <- from + i;
            i += 1;
        }
    }

    public func fan_in(n: i64) -> i64 {
        scope {
            let a = channel(4);
            let b = channel(4);
            let c = channel(4);
            spawn produce(a, 0, n);
            spawn produce(b, 1000, n);
            spawn produce(c, 1000000, n);
            var total = 0;
            var got = 0;
            while got < 3 * n {
                select {
                    v = <-a -> total += v,
                    v = <-b -> total += v,
                    v = <-c -> total += v,
                };
                got += 1;
            };
            total
        }
    }

    public func fairness(rounds: i32) -> i32 {
        scope {
            let a = channel(1);
            let b = channel(1);
            a <- 1;
            b <- 2;
            var from_a = 0;
            var from_b = 0;
            var i = 0;
            while i < rounds {
                select {
                    v = <-a -> {
                        from_a += 1;
                        a <- v
                    },
                    v = <-b -> {
                        from_b += 1;
                        b <- v
                    },
                };
                i += 1;
            };
            if from_a < from_b { from_a } else { from_b }
        }
    }

    public func poll(fill: bool) -> i32 {
        scope {
            let a = channel(1);
            if fill {
                a <- 5
            };
            select {
                v = <-a -> v,
                default -> -1,
            }
        }
    }

    public func try_send() -> i32 {
        scope {
            let a = channel(1);
            var sent = 0;
            var skipped = 0;
            var i = 0;
            while i < 3 {
                select {
                    a <- i -> sent += 1,
                    default -> skipped += 1,
                };
                i += 1;
            };
            sent * 10 + skipped
        }
    }

    func double(input: Channel[i32], output: Channel[i32], n: i32) {
        var i = 0;
        while i < n {
            let x = <-input;
            output <- x * 2;
            i += 1;
        }
    }

    public func pipeline(n: i32) -> i32 {
        scope {
            let input = channel(1);
            let output = channel(1);
            spawn double(input, output, n);
            var next = 0;
            var received = 0;
            var total = 0;
            while received < n {
                if next < n {
                    select {
                        input <- next -> next += 1,
                        v = <-output -> {
                            total += v;
                            received += 1
                        },
                    }
                } else {
                    total += <-output;
                    received += 1;
                }
            };
            total
        }
    }

    func first_of(a: Channel[i64], b: Channel[i64], results: Channel[i64]) {
        let v = select {
            x = <-a -> x,
            y = <-b -> y + 100,
        };
        results <- v
    }

    public func selecting_tasks(n: i64) -> i64 {
        scope {
            let a = channel(1);
            let b = channel(1);
            let results = channel(64);
            var i = 0;
            while i < n {
                spawn first_of(a, b, results);
                i += 1;
            };
            i = 0;
            while i < n {
                if i % 2 == 0 {
                    a <- i
                } else {
                    b <- i
                };
                i += 1;
            };
            var total = 0;
            i = 0;
            while i < n {
                total += <-results;
                i += 1;
            };
            total
        }
    }
    ";

/// Builds the program and runs `test` against it on another thread, failing
/// if the whole thing takes more than two minutes.
fn with_driver(test: impl FnOnce(&CompileAndRunTestDriver) + Send + 'static) {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let driver = CompileAndRunTestDriver::new(SOURCE, |builder| builder)
            .expect("Failed to build test driver");
        test(&driver);
        let _ = done.send(());
    });
    match finished.recv_timeout(Duration::from_secs(120)) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("the test did not finish in two minutes"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the test failed"),
    }
}

/// Three producers, one consumer selecting over their channels: every value
/// arrives exactly once.
#[test]
fn select_fans_in_several_producers() {
    with_driver(|driver| {
        let total: i64 = driver.runtime.invoke("fan_in", (200i64,)).unwrap();
        let n = 200i64;
        let series = n * (n - 1) / 2;
        assert_eq!(
            total,
            series + (1000 * n + series) + (1_000_000 * n + series)
        );
    });
}

/// Both arms are ready on every round; each is chosen a fair share of the
/// time rather than the first listed every time.
#[test]
fn select_is_fair_between_ready_arms() {
    with_driver(|driver| {
        let fewer: i32 = driver.runtime.invoke("fairness", (4000i32,)).unwrap();
        assert!(
            fewer > 1500,
            "one arm was chosen only {fewer} times of 4000"
        );
    });
}

/// `default` runs exactly when no operation can proceed, for receives and
/// for sends.
#[test]
fn select_default_arm() {
    with_driver(|driver| {
        let value: i32 = driver.runtime.invoke("poll", (false,)).unwrap();
        assert_eq!(value, -1);
        let value: i32 = driver.runtime.invoke("poll", (true,)).unwrap();
        assert_eq!(value, 5);
        // The one-slot channel takes the first send; the other two find it
        // full and take `default`.
        let value: i32 = driver.runtime.invoke("try_send", ()).unwrap();
        assert_eq!(value, 12);
    });
}

/// Send and receive arms in one select, against a task that doubles what
/// it is sent: the select feeds it while it can and drains it when it must.
#[test]
fn select_mixes_sends_and_receives() {
    with_driver(|driver| {
        let total: i32 = driver.runtime.invoke("pipeline", (50i32,)).unwrap();
        assert_eq!(total, 2 * (0..50).sum::<i32>());
    });
}

/// Tasks block in a select until the main function sends; each is woken
/// and takes exactly one value.
#[test]
fn tasks_block_in_select() {
    with_driver(|driver| {
        let total: i64 = driver.runtime.invoke("selecting_tasks", (40i64,)).unwrap();
        let evens: i64 = (0..40).filter(|i| i % 2 == 0).sum();
        let odds: i64 = (0..40).filter(|i| i % 2 == 1).map(|i| i + 100).sum();
        assert_eq!(total, evens + odds);
    });
}
