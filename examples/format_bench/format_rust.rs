// The Rust side of the format spec benchmark (src/mod.code is the Codira
// side; both format the same values and print the same check values):
//
//     rustc -C opt-level=3 -o /tmp/format_rust format_rust.rs && /tmp/format_rust
use std::hint::black_box;
use std::time::Instant;

#[derive(Debug)]
#[allow(dead_code)]
struct Point {
    x: i64,
    y: f64,
}

fn value(i: i64) -> f64 {
    ((i * 7919) % 1000003) as f64 / 997.0 - 300.0
}

fn check(s: &str) -> u64 {
    s.len() as u64 + u64::from(s.as_bytes()[s.len() - 1])
}

fn median(mut times: Vec<u128>) -> u128 {
    times.sort_unstable();
    times[3]
}

fn bench(name: &str, mut one: impl FnMut(i64) -> String) {
    let count = 1_000_000;
    let mut times = Vec::new();
    let mut sum = 0u64;
    for _ in 0..7 {
        sum = 0;
        let start = Instant::now();
        for i in 0..count {
            let s = one(black_box(i));
            sum += check(&s);
        }
        times.push(start.elapsed().as_nanos());
    }
    println!("{name} {} {}", median(times), black_box(sum));
}

fn main() {
    // A Codira `Point` prints as `Point { x: .., y: .. }`, with a float's
    // `.0`, which is what `{:?}` writes too.
    bench("float_right_10_3", |i| format!("{:>10.3}", value(i)));
    bench("int_zero_8", |i| format!("{:08}", (i * 7919) % 1000003 - 500000));
    bench("debug_struct", |i| {
        format!("{:?}", Point { x: i, y: value(i) })
    });
}
