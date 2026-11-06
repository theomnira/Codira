// AURORA: formatting, the Rust baseline of `src/mod.code` (the same values,
// the same text, the same check values).
#[path = "../common/bench.rs"]
mod bench;

use std::hint::black_box;

#[derive(Debug)]
#[allow(dead_code)]
struct Point {
    x: i64,
    y: f64,
}

fn value(i: i64) -> f64 {
    ((i * 7919) % 1000003) as f64 / 997.0 - 300.0
}

fn integer(i: i64) -> i64 {
    (i * 7919) % 1000003 - 500000
}

fn check(s: &str) -> u64 {
    s.len() as u64 + u64::from(s.as_bytes()[s.len() - 1])
}

fn run(one: impl Fn(i64) -> String) {
    let start = bench::now();
    let mut sum = 0u64;
    for i in 0..1_000_000 {
        sum += check(&one(black_box(i)));
    }
    bench::report(start, sum);
}

fn main() {
    match bench::kernel().as_str() {
        "int_display" => run(|i| integer(i).to_string()),
        "int_zero_8" => run(|i| format!("{:08}", integer(i))),
        // Codira's `to_string` of a float is Rust's `{:?}` (`1.0`, not `1`).
        "float_display" => run(|i| format!("{:?}", value(i))),
        "float_right_10_3" => run(|i| format!("{:>10.3}", value(i))),
        "debug_struct" => run(|i| format!("{:?}", Point { x: i, y: value(i) })),
        k => bench::unknown(k),
    }
}
