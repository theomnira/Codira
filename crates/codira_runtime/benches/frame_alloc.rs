//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - What frame allocation (`--frame-alloc`) saves on allocation-heavy code:
//!   small temporary objects in a loop, a builder, and a temporary array,
//!   each built with frame allocation off and on, and the same work written
//!   in Rust (with the objects on the stack, and boxed on the heap).
//! - Allocation counts are exact, not estimated: `Runtime::stats()` counts
//!   every object allocated.
//!
//! Run with `cargo bench -p codira_runtime --bench frame_alloc`. Samples
//! alternate between the builds; the report gives the median per loop
//! iteration and checks that every build returns the same checksum. A heap
//! build is timed twice: with no heap limit (allocation only, nothing is
//! collected) and with a 16 MiB limit (allocation and collection). Under
//! `cargo test` one tiny sample runs as a smoke test.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use codira_test::CompileAndRunTestDriver;

const WORKLOADS: &str = r#"
    class Vec2 { x: i64, y: i64 }

    func dot(a: Vec2, b: Vec2) -> i64 { a.x * b.x + a.y * b.y }

    // Two temporary objects per iteration, passed to a function that reads
    // them.
    public func temps(n: i64) -> i64 {
        var total = 0
        for i in 0..n {
            let a = Vec2 { x: i, y: i + 1 }
            let b = Vec2 { x: a.y, y: a.x * 3 }
            total += dot(a, b)
        }
        total
    }

    class Builder { var width: i64, var height: i64, var depth: i64 }

    func with_width(b: Builder, w: i64) -> Builder { b.width = w; b }
    func with_height(b: Builder, h: i64) -> Builder { b.height = h; b }
    func volume(b: Builder) -> i64 { b.width * b.height * b.depth }

    // One builder per iteration, threaded through setters that return it.
    public func builder(n: i64) -> i64 {
        var total = 0
        for i in 0..n {
            let b = with_height(with_width(Builder { width: 1, height: 1, depth: 2 }, i), i + 3)
            total += volume(b)
        }
        total
    }

    // A temporary array of four elements per iteration.
    public func temp_array(n: i64) -> i64 {
        var total = 0
        for i in 0..n {
            let xs = [i, i * 2, i + 7, 3]
            var s = 0
            for x in xs { s += x }
            total += s * xs[1] + xs.len() as i64
        }
        total
    }
"#;

/// Objects each iteration of a workload allocates without frame allocation.
fn objects_per_iteration(workload: &str) -> usize {
    match workload {
        "temps" => 2,
        "builder" | "temp_array" => 1,
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy)]
struct Vec2 {
    x: i64,
    y: i64,
}

#[inline(never)]
fn dot(a: &Vec2, b: &Vec2) -> i64 {
    a.x.wrapping_mul(b.x).wrapping_add(a.y.wrapping_mul(b.y))
}

fn rust_temps(n: i64, boxed: bool) -> i64 {
    let mut total = 0i64;
    for i in 0..n {
        let (a, b);
        if boxed {
            let ab = black_box(Box::new(Vec2 { x: i, y: i + 1 }));
            let bb = black_box(Box::new(Vec2 {
                x: ab.y,
                y: ab.x.wrapping_mul(3),
            }));
            a = *ab;
            b = *bb;
        } else {
            a = Vec2 { x: i, y: i + 1 };
            b = Vec2 {
                x: a.y,
                y: a.x.wrapping_mul(3),
            };
        }
        total = total.wrapping_add(dot(&a, &b));
    }
    total
}

struct Builder {
    width: i64,
    height: i64,
    depth: i64,
}

fn rust_builder(n: i64, boxed: bool) -> i64 {
    let mut total = 0i64;
    for i in 0..n {
        let v = if boxed {
            let mut b = black_box(Box::new(Builder {
                width: 1,
                height: 1,
                depth: 2,
            }));
            b.width = i;
            b.height = i + 3;
            b.width.wrapping_mul(b.height).wrapping_mul(b.depth)
        } else {
            let mut b = Builder {
                width: 1,
                height: 1,
                depth: 2,
            };
            b.width = i;
            b.height = i + 3;
            b.width.wrapping_mul(b.height).wrapping_mul(b.depth)
        };
        total = total.wrapping_add(v);
    }
    total
}

fn rust_temp_array(n: i64, boxed: bool) -> i64 {
    let mut total = 0i64;
    for i in 0..n {
        let xs: Vec<i64>;
        let stack;
        let xs: &[i64] = if boxed {
            xs = black_box(vec![i, i.wrapping_mul(2), i + 7, 3]);
            &xs
        } else {
            stack = [i, i.wrapping_mul(2), i + 7, 3];
            &stack
        };
        let s: i64 = xs.iter().fold(0i64, |s, x| s.wrapping_add(*x));
        total = total
            .wrapping_add(s.wrapping_mul(xs[1]))
            .wrapping_add(xs.len() as i64);
    }
    total
}

fn rust(workload: &str, n: i64, boxed: bool) -> i64 {
    match workload {
        "temps" => rust_temps(n, boxed),
        "builder" => rust_builder(n, boxed),
        "temp_array" => rust_temp_array(n, boxed),
        _ => unreachable!(),
    }
}

fn median(samples: &mut [Duration], n: i64) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1e9 / n as f64
}

fn main() {
    let bench = std::env::args().any(|arg| arg == "--bench");
    let (samples, n) = if bench {
        (21, 2_000_000i64)
    } else {
        (1, 1_000i64)
    };
    const LIMIT: usize = 16 * 1024 * 1024;

    let heap = CompileAndRunTestDriver::with_frame_allocation(WORKLOADS, false, |b| b)
        .expect("failed to build the heap workloads");
    let heap_limited =
        CompileAndRunTestDriver::with_frame_allocation(WORKLOADS, false, |b| b.heap_limit(LIMIT))
            .expect("failed to build the heap workloads");
    let frame = CompileAndRunTestDriver::with_frame_allocation(WORKLOADS, true, |b| b)
        .expect("failed to build the frame workloads");
    println!("{}", frame.allocation_report());

    println!(
        "{:<11} {:>12} {:>12} | {:>11} {:>11} {:>11} | {:>9} {:>9}",
        "workload",
        "heap allocs",
        "frame allocs",
        "heap ns/it",
        "heap+GC",
        "frame ns/it",
        "rust stk",
        "rust box"
    );
    for workload in ["temps", "builder", "temp_array"] {
        // Exact allocation counts, from the runtime's statistics.
        let counts: Vec<usize> = [&heap, &frame]
            .iter()
            .map(|driver| {
                let before = driver.runtime.stats().objects_allocated;
                let _: i64 = driver.runtime.invoke(workload, (n,)).unwrap();
                (driver.runtime.stats().objects_allocated - before) as usize
            })
            .collect();
        let per_iteration = objects_per_iteration(workload);
        assert_eq!(
            counts[0],
            n as usize * per_iteration,
            "{workload}: heap count"
        );

        let mut times = vec![Vec::new(); 5];
        let mut checksum = None;
        for sample in 0..samples {
            let mut order = [0usize, 1, 2, 3, 4];
            order.rotate_left(sample % 5);
            for which in order {
                let start = Instant::now();
                let result = match which {
                    0 => heap.runtime.invoke(workload, (n,)).unwrap(),
                    1 => heap_limited.runtime.invoke(workload, (n,)).unwrap(),
                    2 => frame.runtime.invoke(workload, (n,)).unwrap(),
                    3 => rust(workload, black_box(n), false),
                    _ => rust(workload, black_box(n), true),
                };
                times[which].push(start.elapsed());
                let result: i64 = black_box(result);
                assert_eq!(
                    *checksum.get_or_insert(result),
                    result,
                    "{workload}: checksum"
                );
            }
            // The unlimited heap grows with every call; start the next
            // sample from an empty one.
            let _ = heap.runtime.collect_blocking();
            let _ = frame.runtime.collect_blocking();
        }
        let medians: Vec<f64> = times.iter_mut().map(|t| median(t, n)).collect();
        println!(
            "{:<11} {:>12} {:>12} | {:>11.2} {:>11.2} {:>11.2} | {:>9.2} {:>9.2}   checksum {}",
            workload,
            counts[0],
            counts[1],
            medians[0],
            medians[1],
            medians[2],
            medians[3],
            medians[4],
            checksum.unwrap(),
        );
    }
    println!("({samples} samples of {n} iterations each; medians)");
}
