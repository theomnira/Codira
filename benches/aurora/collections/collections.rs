// AURORA: arrays and pipelines, the Rust baseline of `src/mod.code`
// (`Vec` and iterators; the same inputs, the same check values).
#[path = "../common/bench.rs"]
mod bench;

use bench::{report, Rng};
use std::hint::black_box;

const N: i64 = 10_000_000;

fn iota() -> Vec<i64> {
    black_box((0..N).collect())
}
fn randoms() -> Vec<i64> {
    let mut r = Rng::new();
    black_box((0..N).map(|_| (r.next() % 1000) as i64).collect())
}
fn floats(seed: u64) -> Vec<f64> {
    let mut r = Rng(seed);
    black_box((0..N).map(|_| (r.next() >> 11) as f64 / 9007199254740992.0).collect())
}

fn main() {
    let start;
    match bench::kernel().as_str() {
        "push_10m" => {
            start = bench::now();
            let mut xs: Vec<i64> = Vec::new();
            for i in 0..black_box(N) {
                xs.push(i);
            }
            report(start, black_box(xs).len() as u64);
        }
        "sum_for" => {
            let xs = iota();
            start = bench::now();
            let mut s = 0i64;
            for x in &xs {
                s = s.wrapping_add(*x);
            }
            report(start, s as u64);
        }
        "sort_1m" => {
            let mut r = Rng::new();
            let mut ys: Vec<i64> = (0..1_000_000).map(|_| (r.next() >> 1) as i64).collect();
            start = bench::now();
            ys.sort_unstable();
            report(start, ys[500000] as u64);
        }
        "map_filter_sum_eager" => {
            let xs = iota();
            start = bench::now();
            let a: Vec<i64> = xs.iter().map(|x| x * 2).collect();
            let b: Vec<i64> = a.into_iter().filter(|x| x % 3 == 0).collect();
            let s: i64 = black_box(b).iter().sum();
            report(start, s as u64);
        }
        "map_filter_sum_lazy" => {
            let xs = iota();
            start = bench::now();
            let s: i64 = xs.iter().map(|x| x * 2).filter(|x| x % 3 == 0).sum();
            report(start, s as u64);
        }
        "map_filter_sum_fold" => {
            let xs = iota();
            start = bench::now();
            let s = xs.iter().fold(0i64, |acc, &x| if (x * 2) % 3 == 0 { acc + x * 2 } else { acc });
            report(start, s as u64);
        }
        "enumerate_filter_count" => {
            let rs = randoms();
            start = bench::now();
            let c = rs.iter().enumerate().filter(|(i, x)| (*i as i64 + **x) % 5 == 0).count();
            report(start, c as u64);
        }
        "zip_fold_dot" => {
            let fs = floats(0x9E3779B97F4A7C15);
            let gs = floats(0x2545F4914F6CDD1D);
            start = bench::now();
            let c = fs.iter().zip(&gs).fold(0.0, |acc, (a, b)| acc + a * b);
            report(start, (c * 1000.0) as u64);
        }
        "take_while_sum" => {
            let xs = iota();
            let n = black_box(N);
            start = bench::now();
            let d: i64 = xs.iter().take_while(|&&x| x < n - 1000).sum();
            report(start, d as u64);
        }
        "map_collect" => {
            let xs = iota();
            start = bench::now();
            let e: Vec<i64> = xs.iter().map(|x| x * 3 + 1).collect();
            let e = black_box(e);
            report(start, (e[e.len() / 2] + e.len() as i64) as u64);
        }
        "filter_collect" => {
            let rs = randoms();
            start = bench::now();
            let f: Vec<i64> = rs.iter().copied().filter(|x| x % 3 == 0).collect();
            let f = black_box(f);
            report(start, (f[f.len() / 2] + f.len() as i64) as u64);
        }
        "range_filter_map_sum" => {
            let n = black_box(N);
            start = bench::now();
            let g: i64 = (0..n).filter(|i| i % 3 == 0).map(|i| i ^ (i >> 3)).sum();
            report(start, g as u64);
        }
        "chain_filter_count" => {
            let xs = iota();
            let rs = randoms();
            start = bench::now();
            let h = xs.iter().chain(&rs).filter(|&&x| x % 7 == 1).count();
            report(start, h as u64);
        }
        k => bench::unknown(k),
    }
}
