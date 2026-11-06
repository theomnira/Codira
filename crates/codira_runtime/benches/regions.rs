//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - What `@region` saves on the allocations frame allocation cannot take:
//!   a linked list built in a loop (each node kept past its iteration), a
//!   temporary `array_filled` array, and the array a `map` builds. Each
//!   workload runs on the GC heap, with `--frame-alloc`, and as a `@region`
//!   function (with `--frame-alloc`), and in Rust with `Box`/`Vec`.
//! - Allocation counts are exact: `Runtime::stats()` counts heap objects and
//!   `Runtime::region_stats()` region objects.
//!
//! Run with `cargo bench -p codira_runtime --bench regions`. Samples
//! alternate between the builds; the report gives the median per call of
//! the region function and checks that every build returns the same
//! checksum. Release builds of the runtime do not check regions at close
//! (debug builds do). Under `cargo test` one tiny sample runs.

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

use codira_test::CompileAndRunTestDriver;

const WORKLOADS: &str = r#"
    class Cell { var value: i64, var next: Cell? }

    // A list of `n` cells, built in a loop and summed: every cell is kept
    // past its iteration, so none can be in the frame.
    ANNOTATION
    func list_sum(n: i64) -> i64 {
        var head: Cell? = nil
        for i in 0..n {
            let c = Cell { value: i, next: head }
            head = c
        }
        var total = 0
        var cur = head
        while cur != nil {
            let c = cur!
            total += c.value
            cur = c.next
        }
        total
    }

    // A temporary array whose length is known only at run time.
    ANNOTATION
    func window(n: i64) -> i64 {
        let xs = array_filled(3, n as usize)
        var s = 0
        for x in xs { s += x }
        s + xs.len() as i64
    }

    // The array `map` builds.
    ANNOTATION
    func mapped(n: i64) -> i64 {
        let xs = array_filled(5, n as usize)
        let ys = xs.map(func(x) { x * 2 + 1 })
        var s = 0
        for y in ys { s += y }
        s
    }

    public func lists(calls: i64, n: i64) -> i64 {
        var t = 0
        for i in 0..calls { t += list_sum(n + i % 2) }
        t
    }

    public func windows(calls: i64, n: i64) -> i64 {
        var t = 0
        for i in 0..calls { t += window(n + i % 2) }
        t
    }

    public func maps(calls: i64, n: i64) -> i64 {
        var t = 0
        for i in 0..calls { t += mapped(n + i % 2) }
        t
    }
"#;

struct Cell {
    value: i64,
    next: Option<Box<Cell>>,
}

fn rust(workload: &str, calls: i64, n: i64) -> i64 {
    let mut t = 0i64;
    for i in 0..calls {
        let n = n + i % 2;
        t = t.wrapping_add(match workload {
            "lists" => {
                let mut head: Option<Box<Cell>> = None;
                for i in 0..n {
                    head = Some(black_box(Box::new(Cell {
                        value: i,
                        next: head,
                    })));
                }
                let mut total = 0i64;
                let mut cur = head.as_deref();
                while let Some(c) = cur {
                    total += c.value;
                    cur = c.next.as_deref();
                }
                total
            }
            "windows" => {
                let xs = black_box(vec![3i64; n as usize]);
                xs.iter().sum::<i64>() + xs.len() as i64
            }
            _ => {
                let xs = black_box(vec![5i64; n as usize]);
                let ys: Vec<i64> = black_box(xs.iter().map(|x| x * 2 + 1).collect());
                ys.iter().sum()
            }
        });
    }
    t
}

fn median(samples: &mut [Duration], calls: i64) -> f64 {
    samples.sort();
    samples[samples.len() / 2].as_secs_f64() * 1e9 / calls as f64
}

fn main() {
    let bench = std::env::args().any(|arg| arg == "--bench");
    let (samples, calls, n) = if bench {
        (21, 20_000i64, 64i64)
    } else {
        (1, 10i64, 8i64)
    };
    let build = |region: bool, frame: bool| {
        let source = WORKLOADS.replace("ANNOTATION", if region { "@region" } else { "" });
        CompileAndRunTestDriver::with_frame_allocation(&source, frame, |b| b)
            .expect("failed to build the workloads")
    };
    let heap = build(false, false);
    let frame = build(false, true);
    let region = build(true, true);
    println!("{}", region.allocation_report());

    println!(
        "{:<8} {:>11} {:>11} {:>11} {:>12} | {:>10} {:>10} {:>10} {:>9}",
        "workload",
        "heap objs",
        "frame objs",
        "region heap",
        "region objs",
        "heap ns",
        "frame ns",
        "region ns",
        "rust ns"
    );
    for workload in ["lists", "windows", "maps"] {
        let counts: Vec<(u64, u64)> = [&heap, &frame, &region]
            .iter()
            .map(|d| {
                let (h, r) = (
                    d.runtime.stats().objects_allocated,
                    d.runtime.region_stats().objects_allocated,
                );
                let _: i64 = d.runtime.invoke(workload, (calls, n)).unwrap();
                (
                    d.runtime.stats().objects_allocated - h,
                    d.runtime.region_stats().objects_allocated - r,
                )
            })
            .collect();
        let mut times = vec![Vec::new(); 4];
        let mut checksum = None;
        for sample in 0..samples {
            let mut order = [0usize, 1, 2, 3];
            order.rotate_left(sample % 4);
            for which in order {
                let start = Instant::now();
                let result: i64 = match which {
                    0 => heap.runtime.invoke(workload, (calls, n)).unwrap(),
                    1 => frame.runtime.invoke(workload, (calls, n)).unwrap(),
                    2 => region.runtime.invoke(workload, (calls, n)).unwrap(),
                    _ => rust(workload, black_box(calls), black_box(n)),
                };
                times[which].push(start.elapsed());
                let result = black_box(result);
                assert_eq!(
                    *checksum.get_or_insert(result),
                    result,
                    "{workload}: checksum"
                );
            }
        }
        let medians: Vec<f64> = times.iter_mut().map(|t| median(t, calls)).collect();
        println!(
            "{:<8} {:>11} {:>11} {:>11} {:>12} | {:>10.1} {:>10.1} {:>10.1} {:>9.1}   checksum {}",
            workload,
            counts[0].0,
            counts[1].0,
            counts[2].0,
            counts[2].1,
            medians[0],
            medians[1],
            medians[2],
            medians[3],
            checksum.unwrap(),
        );
    }
    let stats = region.runtime.region_stats();
    println!(
        "regions: {} opened, {} closed, {} chunks ({} bytes) allocated, {} registered now",
        stats.opened,
        stats.closed,
        stats.chunks_allocated,
        stats.chunk_bytes_allocated,
        stats.live_ranges
    );
    println!("({samples} samples of {calls} calls each, n = {n}; medians per call)");
}
