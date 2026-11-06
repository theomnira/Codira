// The Rust side of the lazy pipeline benchmarks (src/mod.code is the
// Codira side; both print the same checksums):
// rustc -C opt-level=3 pipelines_rust.rs
use std::hint::black_box;
use std::time::Instant;

fn report(name: &str, start: Instant, check: i64) {
    println!("{name} {:.3} ms ({check})", start.elapsed().as_secs_f64() * 1e3);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn main() {
    let n: i64 = black_box(10_000_000);

    let mut xs: Vec<i64> = Vec::new();
    for i in 0..n {
        xs.push(i);
    }
    let xs = black_box(xs);
    let mut r = Rng(0x9E3779B97F4A7C15);
    let mut rs: Vec<i64> = Vec::new();
    let mut fs: Vec<f64> = Vec::new();
    let mut gs: Vec<f64> = Vec::new();
    for _ in 0..n {
        rs.push((r.next() % 1000) as i64);
        fs.push((r.next() >> 11) as f64 / 9007199254740992.0);
        gs.push((r.next() >> 11) as f64 / 9007199254740992.0);
    }
    let (rs, fs, gs) = black_box((rs, fs, gs));

    let t = Instant::now();
    let a1: i64 = xs.iter().map(|x| x * 2).filter(|x| x % 3 == 0).sum();
    report("map_filter_sum_lazy", t, black_box(a1));

    let t = Instant::now();
    let b = rs
        .iter()
        .enumerate()
        .filter(|(i, x)| (*i as i64 + **x) % 5 == 0)
        .count();
    report("enumerate_filter_count", t, black_box(b) as i64);

    let t = Instant::now();
    let c = fs.iter().zip(&gs).fold(0.0, |acc, (a, b)| acc + a * b);
    report("zip_fold_dot", t, (black_box(c) * 1000.0) as i64);

    let t = Instant::now();
    let d: i64 = xs.iter().take_while(|&&x| x < n - 1000).sum();
    report("take_while_sum", t, black_box(d));

    let t = Instant::now();
    let e: Vec<i64> = xs.iter().map(|x| x * 3 + 1).collect();
    let e = black_box(e);
    report("map_collect", t, e[e.len() / 2] + e.len() as i64);
    let t = Instant::now();
    let f: Vec<i64> = rs.iter().copied().filter(|x| x % 3 == 0).collect();
    let f = black_box(f);
    report("filter_collect", t, f[f.len() / 2] + f.len() as i64);

    let t = Instant::now();
    let g: i64 = (0..n).filter(|i| i % 3 == 0).map(|i| i ^ (i >> 3)).sum();
    report("range_filter_map_sum", t, black_box(g));
    let t = Instant::now();
    let h = xs.iter().chain(&rs).filter(|&&x| x % 7 == 1).count();
    report("chain_filter_count", t, black_box(h) as i64);
}
