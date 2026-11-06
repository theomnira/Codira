// AURORA: numeric loops and calls, the Rust baseline of `src/mod.code`
// (the same arithmetic, wrapping where Codira wraps, checked indexing).
#[path = "../common/bench.rs"]
mod bench;

use bench::report;
use std::hint::black_box;

const N: usize = 10_000_000;

fn ramp() -> Vec<i64> {
    black_box((0..N).map(|i| (i as i64) * 3 - 7).collect())
}

struct Acc {
    total: i64,
}

#[inline(never)]
fn store_into(xs: &[i64], acc: &mut Acc) {
    for x in xs {
        acc.total = acc.total.wrapping_add(*x);
    }
}

#[inline(never)]
fn copy_into(xs: &[i64], ys: &mut [i64]) {
    for (k, x) in xs.iter().enumerate() {
        ys[k] = x + 1;
    }
}

fn fib(n: i64) -> i64 {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    let start;
    match bench::kernel().as_str() {
        "sum_for" => {
            let xs = ramp();
            start = bench::now();
            let mut t = 0i64;
            for x in &xs {
                t = t.wrapping_add(*x);
            }
            report(start, t as u64);
        }
        "sum_indexed" => {
            let xs = ramp();
            start = bench::now();
            let mut t = 0i64;
            let mut i = 0;
            while i < xs.len() {
                t = t.wrapping_add(xs[i]);
                i += 1;
            }
            report(start, t as u64);
        }
        "store_field" => {
            let xs = ramp();
            let mut acc = Box::new(Acc { total: 0 });
            start = bench::now();
            store_into(&xs, &mut acc);
            report(start, acc.total as u64);
        }
        "copy_plus_one" => {
            let xs = ramp();
            // Not `vec![0; N]`: that is `calloc`, whose pages would be
            // faulted in inside the timed loop; Codira's `array_filled`
            // and C++'s vector write theirs before it starts.
            let mut ys = vec![-1i64; N];
            start = bench::now();
            copy_into(&xs, &mut ys);
            report(start, (ys[7] + ys[N - 1]) as u64);
        }
        "fib_32" => {
            start = bench::now();
            report(start, fib(black_box(32)) as u64);
        }
        "sieve_20m" => {
            let n = black_box(20_000_000usize);
            start = bench::now();
            let mut composite = vec![false; n];
            let mut count = 0u64;
            let mut i = 2;
            while i < n {
                if !composite[i] {
                    count += 1;
                    let mut j = i * i;
                    while j < n {
                        composite[j] = true;
                        j += i;
                    }
                }
                i += 1;
            }
            report(start, count);
        }
        "mandelbrot" => {
            start = bench::now();
            let (mut inside, mut steps) = (0u64, 0u64);
            for py in 0..black_box(400) {
                for px in 0..600 {
                    let cx = px as f64 / 200.0 - 2.0;
                    let cy = py as f64 / 200.0 - 1.0;
                    let (mut x, mut y) = (0.0f64, 0.0f64);
                    let mut k = 0;
                    while k < 200 && x * x + y * y <= 4.0 {
                        let t = x * x - y * y + cx;
                        y = 2.0 * x * y + cy;
                        x = t;
                        k += 1;
                    }
                    steps += k as u64;
                    if k == 200 {
                        inside += 1;
                    }
                }
            }
            report(start, inside * 1_000_000_000 + steps);
        }
        "matmul_200" => {
            let n = black_box(200usize);
            let a: Vec<f64> = (0..n * n).map(|i| (i % 17) as f64 * 0.25).collect();
            let b: Vec<f64> = (0..n * n).map(|i| (i % 13) as f64 * 0.5).collect();
            let mut c = vec![0.0f64; n * n];
            start = bench::now();
            for i in 0..n {
                for k in 0..n {
                    let aik = a[i * n + k];
                    for j in 0..n {
                        c[i * n + j] += aik * b[k * n + j];
                    }
                }
            }
            let mut s = 0.0;
            for v in &c {
                s += v;
            }
            report(start, s as u64);
        }
        "collatz_1m" => {
            start = bench::now();
            let (mut best, mut at) = (0u64, 0u64);
            for n in 1..black_box(1_000_000u64) {
                let mut x = n;
                let mut steps = 0u64;
                while x != 1 {
                    x = if x & 1 == 0 { x >> 1 } else { x.wrapping_mul(3).wrapping_add(1) };
                    steps += 1;
                }
                if steps > best {
                    best = steps;
                    at = n;
                }
            }
            report(start, at * 1000 + best);
        }
        k => bench::unknown(k),
    }
}
