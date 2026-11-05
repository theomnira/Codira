// The Rust side of the growable-array and map benchmarks (src/mod.code is
// the Codira side; both print the same checksums):
// rustc -C opt-level=3 collections_rust.rs
use std::collections::HashMap;
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

/// The finalizer of MurmurHash3, as `std.hashlib.hash` hashes integers.
#[derive(Default)]
struct Mix64(u64);
impl std::hash::Hasher for Mix64 {
    fn finish(&self) -> u64 {
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51AFD7ED558CCD);
        x ^= x >> 33;
        x = x.wrapping_mul(0xC4CEB9FE1A85EC53);
        x ^ (x >> 33)
    }
    fn write(&mut self, _: &[u8]) {
        unimplemented!()
    }
    fn write_i64(&mut self, v: i64) {
        self.0 = v as u64;
    }
}
type FastMap = HashMap<i64, i64, std::hash::BuildHasherDefault<Mix64>>;

fn main() {
    let n: i64 = black_box(10_000_000);

    let t = Instant::now();
    let mut xs: Vec<i64> = Vec::new();
    for i in 0..n {
        xs.push(i);
    }
    let xs = black_box(xs);
    report("push_10m", t, xs.len() as i64);

    let t = Instant::now();
    let s1: i64 = xs.iter().sum();
    report("sum_iter", t, black_box(s1));
    let t = Instant::now();
    let s2 = xs.iter().fold(0i64, |acc, x| acc.wrapping_add(*x));
    report("sum_fold_closure", t, black_box(s2));
    let t = Instant::now();
    let mut s3 = 0i64;
    for x in &xs {
        s3 = s3.wrapping_add(*x);
    }
    report("sum_for_loop", t, black_box(s3));
    let t = Instant::now();
    let s5: i64 = xs.iter().map(|x| x * 2).filter(|x| x % 3 == 0).sum();
    report("map_filter_sum", t, black_box(s5));

    let mut r = Rng(0x9E3779B97F4A7C15);
    let mut ys: Vec<i64> = (0..1_000_000).map(|_| (r.next() >> 1) as i64).collect();
    let t = Instant::now();
    ys.sort_unstable();
    report("sort_unstable_1m", t, ys[500000]);
    let mut r2 = Rng(0x1234_5678_9abc_def1);
    let mut zs: Vec<i64> = (0..1_000_000).map(|_| (r2.next() >> 1) as i64).collect();
    let t = Instant::now();
    zs.sort();
    report("sort_stable_1m", t, zs[500000]);

    let keys: Vec<i64> = (0..1_000_000).map(|_| (r.next() >> 1) as i64).collect();
    let t = Instant::now();
    let mut m: HashMap<i64, i64> = HashMap::new();
    for &k in &keys {
        m.insert(k, k ^ 5);
    }
    report("map_insert_1m_siphash", t, m.len() as i64);
    let t = Instant::now();
    let mut found = 0i64;
    for k in &keys {
        if let Some(v) = m.get(k) {
            found += v & 1;
        }
    }
    report("map_lookup_1m_siphash", t, black_box(found));

    let t = Instant::now();
    let mut f: FastMap = FastMap::default();
    for &k in &keys {
        f.insert(k, k ^ 5);
    }
    report("map_insert_1m_mix64", t, f.len() as i64);
    let t = Instant::now();
    let mut found = 0i64;
    for k in &keys {
        if let Some(v) = f.get(k) {
            found += v & 1;
        }
    }
    report("map_lookup_1m_mix64", t, black_box(found));
}
