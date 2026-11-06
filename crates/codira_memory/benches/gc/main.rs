//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - Allocation and collection benchmark: the size-class heap (`MarkSweep`)
//!   against a copy of the collector it replaced (`legacy.rs`).
//!
//! Run with `cargo bench -p codira_memory --bench gc`. Variants are
//! interleaved (the order alternates every sample) and every workload reports
//! the minimum and the median over all samples, because the machine this is
//! developed on is never idle. Under `cargo test` (no `--bench` argument) a
//! single tiny sample of every workload runs as a smoke test.
mod legacy;

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use codira_memory::{
    gc::{Array, Event, GcPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver},
    HasStaticType, StructTypeBuilder, Type,
};
use legacy::LegacyMarkSweep;

type New = MarkSweep<NoopObserver<Event>>;

struct Config {
    samples: usize,
    objects: usize,
    threads: usize,
}

struct Types {
    leaf: Type,
    pair: Type,
    pairs: Type,
}

fn types() -> Types {
    let leaf = StructTypeBuilder::new("Leaf")
        .add_field("value", i64::type_info().clone())
        .finish();
    let pair = StructTypeBuilder::new("Pair")
        .add_field("a", leaf.clone())
        .add_field("b", leaf.clone())
        .finish();
    let pairs = pair.array_type();
    Types { leaf, pair, pairs }
}

/// A heap interface over both collectors.
trait Heap: Send + Sync + Default + 'static {
    const NAME: &'static str;
    fn alloc(&self, ty: &Type) -> GcPtr;
    fn alloc_array(&self, ty: &Type, n: usize) -> GcPtr;
    fn root(&self, handle: GcPtr);
    fn collect(&self) -> bool;
}

impl Heap for New {
    const NAME: &'static str = "size-class heap";
    fn alloc(&self, ty: &Type) -> GcPtr {
        GcRuntime::alloc(self, ty)
    }
    fn alloc_array(&self, ty: &Type, n: usize) -> GcPtr {
        GcRuntime::alloc_array(self, ty, n).as_raw()
    }
    fn root(&self, handle: GcPtr) {
        GcRuntime::root(self, handle);
    }
    fn collect(&self) -> bool {
        MarkSweep::collect_blocking(self).is_ok_and(|report| report.reclaimed_anything())
    }
}

impl Heap for LegacyMarkSweep {
    const NAME: &'static str = "previous (HashMap)";
    fn alloc(&self, ty: &Type) -> GcPtr {
        LegacyMarkSweep::alloc(self, ty)
    }
    fn alloc_array(&self, ty: &Type, n: usize) -> GcPtr {
        LegacyMarkSweep::alloc_array(self, ty, n)
    }
    fn root(&self, handle: GcPtr) {
        LegacyMarkSweep::root(self, handle);
    }
    fn collect(&self) -> bool {
        LegacyMarkSweep::collect(self)
    }
}

/// Allocates `n` leaves into a fresh heap; returns the time of the loop.
fn alloc_cold<H: Heap>(types: &Types, n: usize) -> Duration {
    let heap = H::default();
    let start = Instant::now();
    for _ in 0..n {
        black_box(heap.alloc(&types.leaf));
    }
    let elapsed = start.elapsed();
    drop(heap);
    elapsed
}

/// Allocates `n` leaves into a heap whose previous `n` leaves were just
/// collected, so memory is reused instead of requested from the system.
fn alloc_warm<H: Heap>(types: &Types, n: usize) -> Duration {
    let heap = H::default();
    for _ in 0..n {
        heap.alloc(&types.leaf);
    }
    heap.collect();
    let start = Instant::now();
    for _ in 0..n {
        black_box(heap.alloc(&types.leaf));
    }
    start.elapsed()
}

/// Allocates `n` leaves split over `threads` threads; returns wall time.
fn alloc_threads<H: Heap>(types: &Types, n: usize, threads: usize) -> Duration {
    let heap = Arc::new(H::default());
    let per_thread = n / threads;
    let barrier = Arc::new(std::sync::Barrier::new(threads + 1));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let heap = heap.clone();
            let leaf = types.leaf.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..per_thread {
                    black_box(heap.alloc(&leaf));
                }
                barrier.wait();
            })
        })
        .collect();
    barrier.wait();
    let start = Instant::now();
    barrier.wait();
    let elapsed = start.elapsed();
    for handle in handles {
        handle.join().unwrap();
    }
    elapsed
}

/// Builds a graph of `pairs` live pairs (a rooted array of pairs, each with two
/// leaves) and as much unreachable garbage, then times one collection.
fn collect_graph<H: Heap>(types: &Types, pairs: usize) -> Duration {
    let heap = H::default();
    let build = |heap: &H| {
        let array = heap.alloc_array(&types.pairs, pairs);
        let elements = unsafe { array.deref::<u8>().add(2 * std::mem::size_of::<usize>()) }
            .cast::<GcPtr>()
            .cast_mut();
        for index in 0..pairs {
            let pair = heap.alloc(&types.pair);
            let a = heap.alloc(&types.leaf);
            let b = heap.alloc(&types.leaf);
            unsafe {
                let mut pair_ptr = pair;
                let fields = pair_ptr.deref_mut::<GcPtr>();
                *fields = a;
                *fields.add(1) = b;
                *elements.add(index) = pair;
            }
        }
        array
    };
    let live = build(&heap);
    heap.root(live);
    let _garbage = build(&heap);

    let start = Instant::now();
    assert!(heap.collect());
    start.elapsed()
}

struct Samples {
    name: String,
    unit_divisor: f64,
    unit: &'static str,
    old: Vec<Duration>,
    new: Vec<Duration>,
}

impl Samples {
    fn new(name: impl Into<String>, unit_divisor: f64, unit: &'static str) -> Self {
        Self {
            name: name.into(),
            unit_divisor,
            unit,
            old: Vec::new(),
            new: Vec::new(),
        }
    }

    fn stats(samples: &[Duration], divisor: f64) -> (f64, f64) {
        let mut values: Vec<f64> = samples
            .iter()
            .map(|d| d.as_secs_f64() * 1e9 / divisor)
            .collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (values[0], values[values.len() / 2])
    }

    fn report(&self) {
        let (old_min, old_median) = Self::stats(&self.old, self.unit_divisor);
        let (new_min, new_median) = Self::stats(&self.new, self.unit_divisor);
        let scale = if self.unit == "ms" { 1e-6 } else { 1.0 };
        println!(
            "{:<44} {:>10.2} {:>10.2} | {:>10.2} {:>10.2} | {:>6.2}x {:>6.2}x  ({}, {} samples)",
            self.name,
            old_min * scale,
            old_median * scale,
            new_min * scale,
            new_median * scale,
            old_min / new_min,
            old_median / new_median,
            self.unit,
            self.old.len(),
        );
    }
}

/// Runs `old` and `new` `samples` times each, alternating which goes first.
fn interleave(
    samples: usize,
    result: &mut Samples,
    mut old: impl FnMut() -> Duration,
    mut new: impl FnMut() -> Duration,
) {
    for sample in 0..samples {
        if sample % 2 == 0 {
            result.old.push(old());
            result.new.push(new());
        } else {
            result.new.push(new());
            result.old.push(old());
        }
    }
}

fn main() {
    let bench = std::env::args().any(|arg| arg == "--bench");
    let config = if bench {
        Config {
            samples: 31,
            objects: 200_000,
            threads: 4,
        }
    } else {
        Config {
            samples: 1,
            objects: 1_000,
            threads: 2,
        }
    };
    let types = types();
    let n = config.objects;

    let mut results = Vec::new();

    let mut cold = Samples::new(
        format!("alloc {n} x 8 B struct, fresh heap"),
        n as f64,
        "ns/obj",
    );
    interleave(
        config.samples,
        &mut cold,
        || alloc_cold::<LegacyMarkSweep>(&types, n),
        || alloc_cold::<New>(&types, n),
    );
    results.push(cold);

    let mut warm = Samples::new(
        format!("alloc {n} x 8 B struct, after collect"),
        n as f64,
        "ns/obj",
    );
    interleave(
        config.samples,
        &mut warm,
        || alloc_warm::<LegacyMarkSweep>(&types, n),
        || alloc_warm::<New>(&types, n),
    );
    results.push(warm);

    let threads = config.threads;
    let mut threaded = Samples::new(
        format!("alloc {n} x 8 B struct, {threads} threads (wall)"),
        n as f64,
        "ns/obj",
    );
    interleave(
        config.samples,
        &mut threaded,
        || alloc_threads::<LegacyMarkSweep>(&types, n, threads),
        || alloc_threads::<New>(&types, n, threads),
    );
    results.push(threaded);

    let pairs = n / 6;
    let mut collect = Samples::new(
        format!(
            "collect {} live + {} dead objects",
            3 * pairs + 1,
            3 * pairs + 1
        ),
        1.0,
        "ms",
    );
    interleave(
        config.samples,
        &mut collect,
        || collect_graph::<LegacyMarkSweep>(&types, pairs),
        || collect_graph::<New>(&types, pairs),
    );
    results.push(collect);

    if bench {
        println!(
            "{:<44} {:>10} {:>10} | {:>10} {:>10} | {:>7} {:>7}",
            "workload", "old min", "old med", "new min", "new med", "min", "median"
        );
        println!(
            "{:<44} {:>21} | {:>21} | speedup (old/new)",
            "",
            LegacyMarkSweep::NAME,
            New::NAME
        );
        for result in &results {
            result.report();
        }
    }
}
