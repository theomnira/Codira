//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - What fuel instrumentation costs: the same Codira code built with and
//!   without `--fuel`, timed in alternating order.
//!
//! Run with `cargo bench -p codira_runtime --bench fuel`. Each sample calls
//! the workload once in each build, alternating which goes first, and the
//! report gives min and median per loop iteration. Under `cargo test` (no
//! `--bench` argument) one tiny sample runs as a smoke test.
use std::time::{Duration, Instant};

use codira_test::CompileAndRunTestDriver;

/// A loop whose body calls a small function: two fuel checks per iteration
/// when instrumented (the loop back-edge and `step`'s entry, which LLVM
/// keeps even after inlining because the counter is atomic).
const WORKLOAD: &str = r#"
    func step(x: i64) -> i64 {
        (x * 1103515245 + 12345) % 2147483648
    }

    public func work(n: i64) -> i64 {
        var acc = 0;
        var i = 0;
        while i < n {
            acc = step(acc + i);
            i += 1;
        }
        acc
    }

    public func tight(n: i64) -> i64 {
        var acc = 0;
        var i = 0;
        while i < n {
            acc += i ^ (acc >> 3);
            i += 1;
        }
        acc
    }
"#;

fn time(driver: &CompileAndRunTestDriver, function: &str, n: i64) -> (Duration, i64) {
    let start = Instant::now();
    let result: i64 = driver.runtime.invoke(function, (n,)).unwrap();
    (start.elapsed(), result)
}

fn stats(samples: &mut [Duration], n: i64) -> (f64, f64) {
    samples.sort();
    let per_iteration = |d: Duration| d.as_secs_f64() * 1e9 / n as f64;
    (
        per_iteration(samples[0]),
        per_iteration(samples[samples.len() / 2]),
    )
}

fn main() {
    let bench = std::env::args().any(|arg| arg == "--bench");
    let (samples, n) = if bench {
        (31, 20_000_000i64)
    } else {
        (1, 1_000i64)
    };

    let plain = CompileAndRunTestDriver::new(WORKLOAD, |builder| builder)
        .expect("failed to build the plain workload");
    let fueled = CompileAndRunTestDriver::with_fuel_instrumentation(WORKLOAD, |builder| {
        builder.fuel(u64::MAX / 2)
    })
    .expect("failed to build the instrumented workload");

    if bench {
        println!(
            "{:<10} {:>12} {:>12} | {:>12} {:>12} | {:>9} {:>9}",
            "workload", "plain min", "plain med", "fuel min", "fuel med", "min +%", "med +%"
        );
    }
    for function in ["work", "tight"] {
        let (mut plain_samples, mut fuel_samples) = (Vec::new(), Vec::new());
        for sample in 0..samples {
            let order: [bool; 2] = if sample % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            };
            for instrumented in order {
                let driver = if instrumented { &fueled } else { &plain };
                let (elapsed, result) = time(driver, function, n);
                std::hint::black_box(result);
                if instrumented {
                    fuel_samples.push(elapsed);
                } else {
                    plain_samples.push(elapsed);
                }
            }
        }
        let (plain_min, plain_median) = stats(&mut plain_samples, n);
        let (fuel_min, fuel_median) = stats(&mut fuel_samples, n);
        if bench {
            println!(
                "{:<10} {:>9.3} ns {:>9.3} ns | {:>9.3} ns {:>9.3} ns | {:>8.1}% {:>8.1}%  ({} samples, {} iterations)",
                function,
                plain_min,
                plain_median,
                fuel_min,
                fuel_median,
                (fuel_min / plain_min - 1.0) * 100.0,
                (fuel_median / plain_median - 1.0) * 100.0,
                samples,
                n,
            );
        }
    }
}
