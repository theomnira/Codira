//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Measurements of the experimental Cascade engine: the cost per retain
//!   and release (unpublished and published, against a plain store, naive
//!   atomic reference counting and a side table), the collector's cost per
//!   applied operation, cycle reclamation throughput (against tracing), and
//!   the latency of bounded collector steps.
//!
//! Run with `cargo bench -p codira_memory --bench cascade`. Every number is
//! the median (and minimum) over several samples. Under `cargo test` (no
//! `--bench` argument) a tiny sample of every workload runs as a smoke test.

use std::{
    hint::black_box,
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use codira_memory::{
    gc::{
        cascade::{CascadeConfig, Mutator},
        Event, GcPtr, GcRuntime, HasIndirectionPtr, MarkSweep, NoopObserver,
    },
    HasStaticType, StructTypeBuilder, Type,
};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

type Gc = MarkSweep<NoopObserver<Event>>;

/// Mutator threads in the latency workload: one fewer than the 4 cores of
/// the measuring machine, so the collector thread is not preempted by them.
const LATENCY_MUTATORS: usize = 3;

struct Config {
    samples: usize,
    ops: usize,
    objects: usize,
    latency_time: Duration,
}

struct Types {
    node: Type,
    offset: usize,
}

fn types() -> Types {
    let leaf = StructTypeBuilder::new("BenchLeaf")
        .add_field("value", i64::type_info().clone())
        .finish();
    let node = StructTypeBuilder::new("BenchNode")
        .add_field("value", i64::type_info().clone())
        .add_field("a", leaf.clone())
        .add_field("b", leaf)
        .finish();
    let offset = node.as_struct().unwrap().fields().get(1).unwrap().offset();
    Types { node, offset }
}

fn cascade_runtime() -> Arc<Gc> {
    let gc = Arc::new(Gc::default());
    gc.enable_cascade(CascadeConfig::default());
    gc
}

/// Returns (median, min) of `values`.
fn median_min(values: &mut [f64]) -> (f64, f64) {
    values.sort_by(f64::total_cmp);
    (values[values.len() / 2], values[0])
}

fn report(name: &str, unit: &str, values: &mut [f64]) {
    let (median, min) = median_min(values);
    println!(
        "{name:<58} {median:>10.2} {unit} (min {min:.2}, {} samples)",
        values.len()
    );
}

/// Runs `f` (which performs `ops` operations) `samples` times; returns
/// nanoseconds per operation for each sample.
fn per_op(samples: usize, ops: usize, mut f: impl FnMut()) -> Vec<f64> {
    (0..samples)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_nanos() as f64 / ops as f64
        })
        .collect()
}

fn retain_release(config: &Config) {
    println!("== cost per assignment (one increment + one decrement), ns/op");
    let n = config.ops;
    let t = types();

    // Baseline: a plain store of a reference into a field.
    {
        let slot = AtomicPtr::<u8>::new(std::ptr::null_mut());
        let value = 0x1000 as *mut u8;
        let mut v = per_op(config.samples, n, || {
            for i in 0..n {
                let p = if i & 1 == 0 { value } else { std::ptr::null_mut() };
                slot.store(black_box(p), Ordering::Relaxed);
            }
        });
        report("plain field store (no barrier)", "ns", &mut v);
        let mut v = per_op(config.samples, n, || {
            for i in 0..n {
                let p = if i & 1 == 0 { value } else { std::ptr::null_mut() };
                black_box(slot.swap(black_box(p), Ordering::AcqRel));
            }
        });
        report("atomic swap of the field (published barrier's store)", "ns", &mut v);
    }

    // Naive atomic reference counting: an atomic RMW per retain and release.
    {
        let count = AtomicU32::new(1);
        let mut v = per_op(config.samples, n, || {
            for _ in 0..n {
                count.fetch_add(1, Ordering::Relaxed);
                black_box(count.fetch_sub(1, Ordering::Release));
            }
        });
        report("naive atomic RC, 1 thread", "ns", &mut v);
        let threads = 4;
        let mut v = per_op(config.samples, n, || {
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        for _ in 0..n / threads {
                            count.fetch_add(1, Ordering::Relaxed);
                            black_box(count.fetch_sub(1, Ordering::Release));
                        }
                    });
                }
            });
        });
        report("naive atomic RC, 4 threads, one shared object", "ns", &mut v);
    }

    // A side table keyed by address instead of a header word.
    {
        let table: Mutex<FxHashMap<usize, u32>> = Mutex::new(FxHashMap::default());
        for key in 0..1024usize {
            table.lock().insert(key * 64, 1);
        }
        let mut v = per_op(config.samples, n, || {
            for i in 0..n {
                let key = (i & 1023) * 64;
                *table.lock().get_mut(&key).unwrap() += 1;
                *table.lock().get_mut(&key).unwrap() -= 1;
            }
        });
        report("side table (Mutex<FxHashMap>) RC, 1 thread", "ns", &mut v);
    }

    let gc = cascade_runtime();
    // Unpublished parent and child: the owner counts directly.
    {
        let mut m = gc.cascade_mutator().unwrap();
        let parent = m.alloc(&t.node).unwrap();
        let child = m.alloc(&t.node).unwrap();
        let mut v = per_op(config.samples, n, || {
            for _ in 0..n / 2 {
                m.store(parent, t.offset, Some(child));
                m.store(parent, t.offset, None);
            }
        });
        // Each store is one increment or decrement plus the barrier; two
        // stores make one assignment's worth (inc of new + dec of old).
        for x in &mut v {
            *x *= 2.0;
        }
        report("Cascade store pair, unpublished (owner counts)", "ns", &mut v);
        m.drop_local(parent);
        m.drop_local(child);
        m.safepoint();
    }

    // Published parent and child: the mutator logs, the collector applies.
    {
        let mut m = gc.cascade_mutator().unwrap();
        let parent = m.alloc(&t.node).unwrap();
        let child = m.alloc(&t.node).unwrap();
        m.store(parent, t.offset, Some(child));
        m.publish(parent);
        let mut v = per_op(config.samples, n, || {
            for i in 0..n / 2 {
                m.store(parent, t.offset, Some(child));
                m.store(parent, t.offset, None);
                if i % 4096 == 0 {
                    m.safepoint();
                }
            }
        });
        for x in &mut v {
            *x *= 2.0;
        }
        report("Cascade store pair, published (mutator side)", "ns", &mut v);

        // Collector side: time per applied increment or decrement.
        let mut collector = Vec::new();
        for _ in 0..config.samples {
            for _ in 0..n / 2 {
                m.store(parent, t.offset, Some(child));
                m.store(parent, t.offset, None);
            }
            m.safepoint();
            let before = gc.cascade().unwrap().stats();
            let start = Instant::now();
            for _ in 0..4 {
                m.safepoint();
                gc.cascade_step(usize::MAX).unwrap();
            }
            let elapsed = start.elapsed().as_nanos() as f64;
            let after = gc.cascade().unwrap().stats();
            let applied = (after.applied_incs + after.applied_decs)
                - (before.applied_incs + before.applied_decs);
            collector.push(elapsed / applied.max(1) as f64);
        }
        report(
            "Cascade collector, per applied inc or dec (1 thread)",
            "ns",
            &mut collector,
        );
        m.drop_local(parent);
        m.drop_local(child);
        m.safepoint();
    }

    // Published, four threads storing into their own published objects
    // while one collector thread steps.
    {
        let threads = 4;
        let stop = AtomicBool::new(false);
        let mut v = per_op(config.samples, n, || {
            stop.store(false, Ordering::Relaxed);
            std::thread::scope(|scope| {
                let collector = scope.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        gc.cascade_step(4096).unwrap();
                    }
                });
                let workers: Vec<_> = (0..threads)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut m = gc.cascade_mutator().unwrap();
                            let parent = m.alloc(&t.node).unwrap();
                            let child = m.alloc(&t.node).unwrap();
                            m.store(parent, t.offset, Some(child));
                            m.publish(parent);
                            for i in 0..n / threads / 2 {
                                m.store(parent, t.offset, Some(child));
                                m.store(parent, t.offset, None);
                                if i % 4096 == 0 {
                                    m.safepoint();
                                }
                            }
                            m.drop_local(parent);
                            m.drop_local(child);
                            m.safepoint();
                        })
                    })
                    .collect();
                for worker in workers {
                    worker.join().unwrap();
                }
                stop.store(true, Ordering::Relaxed);
                collector.join().unwrap();
            });
        });
        for x in &mut v {
            *x *= 2.0;
        }
        report(
            "Cascade store pair, published, 4 threads + collector (wall)",
            "ns",
            &mut v,
        );
    }

    // Local references (the Rust API's stand-in for stack slots).
    {
        let mut m = gc.cascade_mutator().unwrap();
        let object = m.alloc(&t.node).unwrap();
        let mut v = per_op(config.samples, n, || {
            for _ in 0..n {
                m.hold(object);
                m.drop_local(object);
            }
        });
        report("Mutator::hold + drop_local (API stand-in only)", "ns", &mut v);
        m.drop_local(object);
        m.safepoint();
    }

    // Unique objects: allocation plus release at the last use, against
    // allocation alone (left to tracing).
    {
        let mut m = gc.cascade_mutator().unwrap();
        let count = n / 4;
        let mut v = per_op(config.samples, count, || {
            for _ in 0..count {
                let object = m.alloc(&t.node).unwrap();
                black_box(m.release_unique(object));
            }
        });
        report("Cascade alloc + release_unique", "ns", &mut v);
        drop(m);
        let plain = Arc::new(Gc::default());
        let mut v = per_op(config.samples, count, || {
            for i in 0..count {
                black_box(plain.alloc(&t.node));
                if i % 65536 == 65535 {
                    plain.collect();
                }
            }
        });
        report("MarkSweep alloc (tracing every 64K objects)", "ns", &mut v);
    }
}

/// Builds `objects` objects as rings of `ring` nodes, published or not,
/// and drops them.
fn build_rings(m: &mut Mutator, t: &Types, objects: usize, ring: usize, publish: bool) {
    for _ in 0..objects / ring {
        let nodes: Vec<GcPtr> = (0..ring).map(|_| m.alloc(&t.node).unwrap()).collect();
        for i in 0..ring {
            m.store(nodes[i], t.offset, Some(nodes[(i + 1) % ring]));
        }
        if publish {
            m.publish(nodes[0]);
        }
        for node in nodes {
            m.drop_local(node);
        }
    }
}

fn cycles(config: &Config) {
    println!("== cycle reclamation throughput");
    let t = types();
    for &ring in &[2usize, 16, 128] {
        let objects = config.objects - config.objects % ring;
        // Published: the concurrent pass, validated a boundary later.
        let mut rates = Vec::new();
        let mut steps_needed = Vec::new();
        for _ in 0..config.samples {
            // Throughput of the pass itself: every candidate fits the queue
            // (the default cap would leave the excess to tracing).
            let gc = Arc::new(Gc::default());
            gc.enable_cascade(CascadeConfig {
                max_candidates: usize::MAX,
                ..CascadeConfig::default()
            });
            let mut m = gc.cascade_mutator().unwrap();
            build_rings(&mut m, &t, objects, ring, true);
            m.safepoint();
            let cascade = gc.cascade().unwrap().clone();
            let start = Instant::now();
            let mut steps = 0;
            while cascade.stats().cycle_reclaimed < objects as u64 && steps < 100_000 {
                m.safepoint();
                gc.cascade_step(1 << 16).unwrap();
                steps += 1;
            }
            let elapsed = start.elapsed().as_secs_f64();
            assert!(cascade.stats().cycle_reclaimed >= objects as u64);
            rates.push(objects as f64 / elapsed / 1e6);
            steps_needed.push(steps as f64);
        }
        report(
            &format!("published rings of {ring}: concurrent pass"),
            "M objects/s",
            &mut rates,
        );
        report(
            &format!("published rings of {ring}: steps (budget 65536)"),
            "steps",
            &mut steps_needed,
        );

        // Unpublished: the owner's synchronous pass.
        let mut rates = Vec::new();
        for _ in 0..config.samples {
            let gc = Arc::new(Gc::default());
            gc.enable_cascade(CascadeConfig {
                local_cycle_budget: usize::MAX,
                ..CascadeConfig::default()
            });
            let mut m = gc.cascade_mutator().unwrap();
            build_rings(&mut m, &t, objects, ring, false);
            let start = Instant::now();
            m.reclaim_local();
            let elapsed = start.elapsed().as_secs_f64();
            assert!(gc.cascade().unwrap().stats().cycle_reclaimed >= objects as u64);
            rates.push(objects as f64 / elapsed / 1e6);
        }
        report(
            &format!("unpublished rings of {ring}: owner's pass"),
            "M objects/s",
            &mut rates,
        );

        // Tracing the same garbage (nothing else live).
        let mut rates = Vec::new();
        for _ in 0..config.samples {
            let gc = Arc::new(Gc::default());
            let roots: Vec<GcPtr> = (0..objects).map(|_| gc.alloc(&t.node)).collect();
            for chunk in roots.chunks(ring) {
                for i in 0..chunk.len() {
                    let mut from = chunk[i];
                    unsafe {
                        *from.deref_mut::<u8>().add(t.offset).cast::<GcPtr>() =
                            chunk[(i + 1) % chunk.len()];
                    }
                }
            }
            drop(roots);
            let start = Instant::now();
            gc.collect();
            let elapsed = start.elapsed().as_secs_f64();
            rates.push(objects as f64 / elapsed / 1e6);
        }
        report(
            &format!("rings of {ring}: one tracing collection"),
            "M objects/s",
            &mut rates,
        );
    }
}

/// CPU time of the current thread, in nanoseconds (0 where unsupported).
fn thread_cpu_nanos() -> u64 {
    #[cfg(unix)]
    unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
    #[cfg(not(unix))]
    0
}

fn latency(config: &Config) {
    println!(
        "== collector step latency under {LATENCY_MUTATORS} mutator threads (published stores, rings); time holding the heap lock"
    );
    let t = types();
    for &budget in &[256usize, 1024, 4096, 16384] {
        let gc = cascade_runtime();
        let stop = AtomicBool::new(false);
        let mut nanos = Vec::new();
        let mut waits = Vec::new();
        let mut cpus = Vec::new();
        let mut work = Vec::new();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..LATENCY_MUTATORS)
                .map(|_| {
                    scope.spawn(|| {
                        let mut m = gc.cascade_mutator().unwrap();
                        let hub = m.alloc(&t.node).unwrap();
                        m.publish(hub);
                        let mut i = 0usize;
                        while !stop.load(Ordering::Relaxed) {
                            // Garbage cycles and published churn.
                            build_rings(&mut m, &t, 8, 4, true);
                            let node = m.alloc(&t.node).unwrap();
                            m.store(hub, t.offset, Some(node));
                            m.drop_local(node);
                            i += 1;
                            if i % 64 == 0 {
                                m.safepoint();
                            }
                        }
                        m.drop_local(hub);
                        m.safepoint();
                    })
                })
                .collect();
            let end = Instant::now() + config.latency_time;
            while Instant::now() < end {
                let cpu = thread_cpu_nanos();
                let report = gc.cascade_step(budget).unwrap();
                let cpu = thread_cpu_nanos() - cpu;
                if report.work > 0 {
                    cpus.push(cpu as f64 / 1000.0);
                    nanos.push(report.nanos as f64 / 1000.0);
                    waits.push(report.wait_nanos as f64 / 1000.0);
                    work.push(report.work as f64);
                }
            }
            stop.store(true, Ordering::Relaxed);
            for worker in workers {
                worker.join().unwrap();
            }
        });
        if nanos.is_empty() {
            continue;
        }
        nanos.sort_by(f64::total_cmp);
        let pick = |q: f64| nanos[((nanos.len() - 1) as f64 * q) as usize];
        let (work_median, _) = median_min(&mut work);
        let (wait_median, _) = median_min(&mut waits);
        let wait_max = waits[waits.len() - 1];
        cpus.sort_by(f64::total_cmp);
        let cpu_p999 = cpus[((cpus.len() - 1) as f64 * 0.999) as usize];
        let cpu_max = cpus[cpus.len() - 1];
        println!(
            "budget {budget:>6}: steps {:>7}  median {:>8.1} us  p99 {:>8.1} us  p99.9 {:>8.1} us  max {:>9.1} us  (median work {work_median}; lock wait median {wait_median:.1} us, max {wait_max:.1} us; thread CPU time p99.9 {cpu_p999:.1} us, max {cpu_max:.1} us)",
            nanos.len(),
            pick(0.5),
            pick(0.99),
            pick(0.999),
            nanos[nanos.len() - 1],
        );
    }
}

fn main() {
    let bench = std::env::args().any(|a| a == "--bench");
    let config = if bench {
        Config {
            samples: 11,
            ops: 4_000_000,
            objects: 200_000,
            latency_time: Duration::from_secs(3),
        }
    } else {
        Config {
            samples: 1,
            ops: 2_000,
            objects: 512,
            latency_time: Duration::from_millis(50),
        }
    };
    retain_release(&config);
    cycles(&config);
    latency(&config);
}
