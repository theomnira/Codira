//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for session types: two tasks talking over the two
//!   endpoints of a session, compiled and run on the runtime's worker pool.
//!
//! Every test runs under a hard timeout, so a protocol that hangs fails the
//! test instead of the suite.
use std::{sync::mpsc, thread, time::Duration};

use codira_test::CompileAndRunTestDriver;

const SOURCE: &str = r"
    session Checkout {
        send i64; recv i64;
        choose { send bool; recv i64 } or { send i32 }
    }

    func shop(ep: dual Checkout) {
        let item = <-ep;
        ep <- item * 100;
        offer ep {
            let pay = <-ep;
            if pay {
                ep <- 1
            } else {
                ep <- 0
            }
        } or {
            let reason = <-ep;
        }
    }

    public func buy(item: i64, budget: i64) -> i64 {
        scope {
            let (me, them) = Checkout.open();
            spawn shop(them);
            me <- item;
            let price = <-me;
            if price <= budget {
                me.choose(0);
                me <- true;
                let receipt = <-me;
                price * 10 + receipt
            } else {
                me.choose(1);
                me <- 7;
                -1
            }
        }
    }

    session Sum {
        send i64
        send i64
        recv i64
    }

    func adder(ep: dual Sum) {
        let a = <-ep;
        let b = <-ep;
        ep <- a + b
    }

    func ask(ep: Sum, a: i64, b: i64) -> i64 {
        ep <- a;
        ep <- b;
        <-ep
    }

    public func add(a: i64, b: i64) -> i64 {
        scope {
            let (client, server) = Sum.open();
            spawn adder(server);
            ask(client, a, b)
        }
    }

    session Halve {
        send f64
        recv f64
        recv bool
    }

    func halver(ep: dual Halve) {
        let x = <-ep;
        ep <- x / 2.0;
        ep <- x > 10.0
    }

    public func halve(x: f64) -> f64 {
        scope {
            let (me, them) = Halve.open();
            spawn halver(them);
            me <- x;
            let half = <-me;
            let big = <-me;
            if big {
                half
            } else {
                0.0 - half
            }
        }
    }

    public func many_sessions(n: i64) -> i64 {
        var total = 0;
        var i = 0;
        while i < n {
            total += add(i, i);
            i += 1;
        };
        total
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

/// The checkout protocol, through both branches of its `choose`.
#[test]
fn checkout_takes_either_branch() {
    with_driver(|driver| {
        let paid: i64 = driver.runtime.invoke("buy", (3i64, 1000i64)).unwrap();
        assert_eq!(paid, 3001);
        let declined: i64 = driver.runtime.invoke("buy", (30i64, 1000i64)).unwrap();
        assert_eq!(declined, -1);
    });
}

/// An endpoint passed to an ordinary function, which finishes its protocol.
#[test]
fn an_endpoint_is_passed_to_a_function() {
    with_driver(|driver| {
        let sum: i64 = driver.runtime.invoke("add", (20i64, 22i64)).unwrap();
        assert_eq!(sum, 42);
    });
}

/// Payloads of different types and sizes at different steps.
#[test]
fn payloads_change_type_between_steps() {
    with_driver(|driver| {
        let value: f64 = driver.runtime.invoke("halve", (30.0f64,)).unwrap();
        assert!((value - 15.0).abs() < f64::EPSILON);
        let value: f64 = driver.runtime.invoke("halve", (4.0f64,)).unwrap();
        assert!((value + 2.0).abs() < f64::EPSILON);
    });
}

/// A session per call, many times over: each pair is freed with its scope.
#[test]
fn sessions_are_opened_and_freed_repeatedly() {
    with_driver(|driver| {
        let total: i64 = driver.runtime.invoke("many_sessions", (200i64,)).unwrap();
        assert_eq!(total, 2 * (0..200).sum::<i64>());
    });
}
