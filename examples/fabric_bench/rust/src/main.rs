//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The Rust programs of the task benchmarks, printing the same `name
//!   nanoseconds check` lines as the Codira host. The thread count comes from
//!   `RAYON_NUM_THREADS`.

use std::time::Instant;

use rayon::prelude::*;

fn mix(i: i64) -> i64 {
    let mut x = i.wrapping_mul(2654435761).wrapping_add(1);
    x ^= x >> 13;
    x = x.wrapping_mul(1274126177);
    x ^ (x >> 16)
}

fn burn(seed: i64, iterations: i64) -> i64 {
    let mut x = seed * 2 + 1;
    for _ in 0..iterations {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    x
}

/// `n` tasks spawned into one scope, each returning its index into a slot,
/// summed after the join (Codira's `group` of `ident`).
fn spawn_join(n: i64) -> i64 {
    let mut slots = vec![0i64; n as usize];
    rayon::scope(|s| {
        for (i, slot) in slots.iter_mut().enumerate() {
            s.spawn(move |_| *slot = std::hint::black_box(i as i64));
        }
    });
    slots.iter().sum()
}

fn fan_out_map(n: i64) -> i64 {
    let all: Vec<i64> = (0..n).into_par_iter().map(mix).collect();
    all.iter()
        .enumerate()
        .fold(0i64, |check, (i, x)| check ^ (x.wrapping_add(i as i64)))
}

fn pipeline(n: i64) -> i64 {
    let (tx, rx) = crossbeam_channel::bounded::<i64>(64);
    let producer = std::thread::spawn(move || {
        for i in 0..n {
            tx.send(i).unwrap();
        }
    });
    let total = (0..n).map(|_| rx.recv().unwrap()).sum();
    producer.join().unwrap();
    total
}

fn compute(tasks: i64, iterations: i64) -> i64 {
    (0..tasks)
        .into_par_iter()
        .map(|t| burn(t, iterations))
        .reduce(|| 0, i64::wrapping_add)
}

fn main() {
    let time = |name: &str, f: &dyn Fn() -> i64| {
        f();
        let start = Instant::now();
        let check = f();
        println!("{name} {} {check}", start.elapsed().as_nanos());
    };
    time("spawn_join_100k", &|| spawn_join(100_000));
    time("fan_out_map_1m", &|| fan_out_map(1_000_000));
    time("pipeline_1m", &|| pipeline(1_000_000));
    time("compute_64x4m", &|| compute(64, 4_000_000));
}
