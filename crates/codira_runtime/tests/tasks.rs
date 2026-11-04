//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - End-to-end tests for structured concurrency: `scope`, `spawn` and
//!   channels, compiled and run on the runtime's worker pool.
//!
//! Every assertion is on values that came back through channels after the
//! tasks that produced them were joined -- never on timing or interleaving.
use codira_test::CompileAndRunTestDriver;

const SOURCE: &str = r"
    effect Fail {
        func fail(code: i32) -> i32
    }

    func produce(dest: Channel[i64], from: i64, count: i64) {
        var i = 0;
        while i < count {
            dest <- from + i;
            i += 1;
        }
    }

    public func producers() -> i64 {
        scope {
            let ch = channel(8);
            spawn produce(ch, 0, 100);
            spawn produce(ch, 1000, 100);
            var total = 0;
            var n = 0;
            while n < 200 {
                total += <-ch;
                n += 1;
            };
            total
        }
    }

    func square(input: Channel[i32], output: Channel[i32]) {
        let x = <-input;
        output <- x * x
    }

    public func fan_out(n: i32) -> i32 {
        scope {
            let jobs = channel(64);
            let results = channel(64);
            var i = 0;
            while i < n {
                spawn square(jobs, results);
                i += 1;
            };
            i = 0;
            while i < n {
                jobs <- i;
                i += 1;
            };
            var sum = 0;
            i = 0;
            while i < n {
                sum += <-results;
                i += 1;
            };
            sum
        }
    }

    func one(dest: Channel[i32]) {
        dest <- 1
    }

    public func early_return(flag: bool) -> i32 {
        scope {
            let ch = channel(1);
            spawn one(ch);
            if flag {
                return 7
            };
            <-ch
        }
    }

    public func break_out() -> i32 {
        var total = 0;
        loop {
            scope {
                let ch = channel(1);
                spawn one(ch);
                total += <-ch;
                if total >= 3 {
                    break
                }
            }
        };
        total
    }

    func work(x: i32) uses Fail -> i32 {
        scope {
            let ch = channel(1);
            spawn one(ch);
            let v = <-ch;
            if x < 0 {
                perform Fail.fail(x)
            } else {
                v + x
            }
        }
    }

    public func aborts(x: i32) -> i32 {
        handle work(x) {
            Fail.fail(c) -> c * 10
        }
    }

    func leaf(dest: Channel[i32], x: i32) {
        dest <- x
    }

    func branch(dest: Channel[i32], x: i32) {
        scope {
            spawn leaf(dest, x);
            spawn leaf(dest, x + 1)
        }
    }

    public func tree() -> i32 {
        scope {
            let ch = channel(16);
            spawn branch(ch, 10);
            spawn branch(ch, 20);
            (<-ch) + (<-ch) + (<-ch) + (<-ch)
        }
    }

    func flag(dest: Channel[bool], value: bool) {
        dest <- value
    }

    func half(dest: Channel[f64], value: f64) {
        dest <- value / 2.0
    }

    public func scalars() -> f64 {
        scope {
            let flags = channel(1);
            let halves = channel(1);
            spawn flag(flags, true);
            spawn half(halves, 5.0);
            let yes = <-flags;
            if yes {
                <-halves
            } else {
                0.0
            }
        }
    }
    ";

fn driver() -> CompileAndRunTestDriver {
    CompileAndRunTestDriver::new(SOURCE, |builder| builder).expect("Failed to build test driver")
}

/// Two producers feed one channel; every value arrives.
#[test]
fn producers_feed_one_channel() {
    let driver = driver();
    let total: i64 = driver.runtime.invoke("producers", ()).unwrap();
    assert_eq!(total, 4950 + 100 * 1000 + 4950);
}

/// Many tasks take jobs from one channel and return results on another.
#[test]
fn fan_out_and_back_in() {
    let driver = driver();
    let sum: i32 = driver.runtime.invoke("fan_out", (10i32,)).unwrap();
    assert_eq!(sum, 285);
    let sum: i32 = driver.runtime.invoke("fan_out", (40i32,)).unwrap();
    assert_eq!(sum, (0..40).map(|i| i * i).sum::<i32>());
}

/// `return`, `break` and an aborting effect handler all leave a scope; each
/// joins its tasks on the way out, and the program carries on normally.
#[test]
fn every_exit_path_joins() {
    let driver = driver();
    let value: i32 = driver.runtime.invoke("early_return", (true,)).unwrap();
    assert_eq!(value, 7);
    let value: i32 = driver.runtime.invoke("early_return", (false,)).unwrap();
    assert_eq!(value, 1);
    let value: i32 = driver.runtime.invoke("break_out", ()).unwrap();
    assert_eq!(value, 3);
    let value: i32 = driver.runtime.invoke("aborts", (5i32,)).unwrap();
    assert_eq!(value, 6);
    let value: i32 = driver.runtime.invoke("aborts", (-2i32,)).unwrap();
    assert_eq!(value, -20);
}

/// A task opens a scope of its own and spawns into it.
#[test]
fn tasks_spawn_tasks() {
    let driver = driver();
    let value: i32 = driver.runtime.invoke("tree", ()).unwrap();
    assert_eq!(value, 62);
}

/// `bool` and float channels.
#[test]
fn scalar_channels() {
    let driver = driver();
    let value: f64 = driver.runtime.invoke("scalars", ()).unwrap();
    assert!((value - 2.5).abs() < f64::EPSILON);
}
