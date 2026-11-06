// The Rust side of the growable-array and map benchmarks (src/mod.code is
// the Codira side; both print the same checksums):
// rustc -C opt-level=3 collections_rust.rs
//
// `secure` maps are the standard library's `HashMap::new()`: hashbrown with
// SipHash-1-3 under a random key, as Codira's `Map.new()`. `fast` maps hash
// with exactly what Codira's `Map.unkeyed()` does: the MurmurHash3
// finalizer of an integer, FNV-1a over a string's bytes. String keys are
// `&str` borrowed from the key array, as a Codira `String` key is a shared
// reference.
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::hint::black_box;
use std::time::Instant;

fn report(name: &str, start: Instant, check: i64) {
    println!("{name} {:.3} ms ({check})", start.elapsed().as_secs_f64() * 1e3);
}

fn report_map(name: &str, ns: u128, ops: i64, check: i64) {
    let ms = ns as f64 / 1e6;
    let per = ns as f64 / ops as f64;
    println!("{name} {ms:.3} ms {per:.2} ns/op ({check})");
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
impl Hasher for Mix64 {
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

/// FNV-1a 64 over a string's bytes, as `std.hashlib.hash` hashes `String`.
/// `Hash for str` also writes a 0xFF terminator byte (`write_u8`), which is
/// left out so that the function is the same as Codira's.
struct Fnv(u64);
impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xCBF29CE484222325)
    }
}
impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001B3);
        }
        self.0 = h;
    }
    fn write_u8(&mut self, _: u8) {}
}

type FastIntMap = HashMap<i64, i64, BuildHasherDefault<Mix64>>;
type FastStrMap<'a> = HashMap<&'a str, i64, BuildHasherDefault<Fnv>>;

/// The operations both map kinds need, so one benchmark body serves both.
trait IntMap {
    fn fresh() -> Self;
    fn ins(&mut self, k: i64, v: i64);
    fn get_(&self, k: i64) -> Option<i64>;
    fn has(&self, k: i64) -> bool;
    fn rem(&mut self, k: i64) -> Option<i64>;
    fn len_(&self) -> usize;
    fn fold(&self) -> i64;
}

macro_rules! int_map {
    ($t:ty, $new:expr) => {
        impl IntMap for $t {
            fn fresh() -> Self {
                $new
            }
            fn ins(&mut self, k: i64, v: i64) {
                self.insert(k, v);
            }
            fn get_(&self, k: i64) -> Option<i64> {
                self.get(&k).copied()
            }
            fn has(&self, k: i64) -> bool {
                self.contains_key(&k)
            }
            fn rem(&mut self, k: i64) -> Option<i64> {
                self.remove(&k)
            }
            fn len_(&self) -> usize {
                self.len()
            }
            fn fold(&self) -> i64 {
                let mut check = 0i64;
                for (k, v) in self {
                    check += (k ^ v) & 7;
                }
                check
            }
        }
    };
}
int_map!(HashMap<i64, i64>, HashMap::new());
int_map!(FastIntMap, FastIntMap::default());

trait StrMap<'a> {
    fn fresh() -> Self;
    fn ins(&mut self, k: &'a str, v: i64);
    fn get_(&self, k: &str) -> Option<i64>;
    fn has(&self, k: &str) -> bool;
    fn rem(&mut self, k: &str) -> Option<i64>;
    fn len_(&self) -> usize;
    fn fold(&self) -> i64;
}

macro_rules! str_map {
    ($t:ty, $new:expr) => {
        impl<'a> StrMap<'a> for $t {
            fn fresh() -> Self {
                $new
            }
            fn ins(&mut self, k: &'a str, v: i64) {
                self.insert(k, v);
            }
            fn get_(&self, k: &str) -> Option<i64> {
                self.get(k).copied()
            }
            fn has(&self, k: &str) -> bool {
                self.contains_key(k)
            }
            fn rem(&mut self, k: &str) -> Option<i64> {
                self.remove(k)
            }
            fn len_(&self) -> usize {
                self.len()
            }
            fn fold(&self) -> i64 {
                let mut check = 0i64;
                for (k, v) in self {
                    check += (k.len() as i64 + v) & 7;
                }
                check
            }
        }
    };
}
str_map!(HashMap<&'a str, i64>, HashMap::new());
str_map!(FastStrMap<'a>, FastStrMap::default());

fn bench_int<M: IntMap>(label: &str, keys: &[i64], misses: &[i64], reps: i64) {
    let ops = keys.len() as i64 * reps;
    let mut ns = 0u128;
    let mut check = 0i64;
    let mut m = M::fresh();
    for _ in 0..reps {
        let t = Instant::now();
        m = M::fresh();
        for &k in keys {
            m.ins(k, k ^ 5);
        }
        ns += t.elapsed().as_nanos();
        check += m.len_() as i64;
    }
    report_map(&format!("map_{label}_insert"), ns, ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        for &k in keys {
            if let Some(v) = m.get_(black_box(k)) {
                check += v & 1;
            }
        }
    }
    report_map(&format!("map_{label}_hit"), t.elapsed().as_nanos(), ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        for &k in misses {
            if m.has(black_box(k)) {
                check += 1;
            }
        }
    }
    report_map(&format!("map_{label}_miss"), t.elapsed().as_nanos(), ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        check += black_box(&m).fold();
    }
    report_map(&format!("map_{label}_iter"), t.elapsed().as_nanos(), ops, check);
    let mut ns = 0u128;
    let mut check = 0i64;
    for rep in 0..reps {
        if rep > 0 {
            for &k in keys {
                m.ins(k, k ^ 5);
            }
        }
        let t = Instant::now();
        for &k in keys {
            if let Some(v) = m.rem(black_box(k)) {
                check += v & 1;
            }
        }
        ns += t.elapsed().as_nanos();
    }
    report_map(
        &format!("map_{label}_remove"),
        ns,
        ops,
        check + m.len_() as i64,
    );
}

fn bench_string<'a, M: StrMap<'a>>(label: &str, keys: &'a [String], misses: &[String], reps: i64) {
    let ops = keys.len() as i64 * reps;
    let mut ns = 0u128;
    let mut check = 0i64;
    let mut m = M::fresh();
    for _ in 0..reps {
        let t = Instant::now();
        m = M::fresh();
        for (i, k) in keys.iter().enumerate() {
            // A `String` key is a reference in Codira, which the map
            // shares; a `&str` key is the same here (no copy of the bytes).
            m.ins(k, i as i64);
        }
        ns += t.elapsed().as_nanos();
        check += m.len_() as i64;
    }
    report_map(&format!("map_{label}_insert"), ns, ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        for k in keys {
            if let Some(v) = m.get_(black_box(k)) {
                check += v & 1;
            }
        }
    }
    report_map(&format!("map_{label}_hit"), t.elapsed().as_nanos(), ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        for k in misses {
            if m.has(black_box(k)) {
                check += 1;
            }
        }
    }
    report_map(&format!("map_{label}_miss"), t.elapsed().as_nanos(), ops, check);
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..reps {
        check += black_box(&m).fold();
    }
    report_map(&format!("map_{label}_iter"), t.elapsed().as_nanos(), ops, check);
    let mut ns = 0u128;
    let mut check = 0i64;
    for rep in 0..reps {
        if rep > 0 {
            for (i, k) in keys.iter().enumerate() {
                m.ins(k, i as i64);
            }
        }
        let t = Instant::now();
        for k in keys {
            if let Some(v) = m.rem(black_box(k)) {
                check += v & 1;
            }
        }
        ns += t.elapsed().as_nanos();
    }
    report_map(
        &format!("map_{label}_remove"),
        ns,
        ops,
        check + m.len_() as i64,
    );
}

fn bench_small<M: IntMap>(name: &str) {
    let keys: [i64; 5] = [17, 4242, -9, 123456789, 77];
    let t = Instant::now();
    let mut check = 0i64;
    for _ in 0..200_000 {
        let mut m = M::fresh();
        for &k in &keys {
            m.ins(black_box(k), k ^ 5);
        }
        check += m.len_() as i64;
    }
    report_map(
        &format!("map_small5_{name}_build"),
        t.elapsed().as_nanos(),
        1_000_000,
        check,
    );
    let mut m = M::fresh();
    for &k in &keys {
        m.ins(k, k ^ 5);
    }
    let t = Instant::now();
    let mut check = 0i64;
    for i in 0..1_000_000i64 {
        let k = keys[(i % 8) as usize % 5] + (i & 8);
        if let Some(v) = m.get_(black_box(k)) {
            check += v & 3;
        }
    }
    report_map(
        &format!("map_small5_{name}_get"),
        t.elapsed().as_nanos(),
        1_000_000,
        check,
    );
}

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

    for size in [1000i64, 100_000, 1_000_000] {
        let reps = 1_000_000 / size;
        let keys: Vec<i64> = (0..size).map(|_| ((r.next() >> 2) * 2) as i64).collect();
        let misses: Vec<i64> = (0..size)
            .map(|_| ((r.next() >> 2) * 2 + 1) as i64)
            .collect();
        let skeys: Vec<String> = keys.iter().map(|k| format!("key-{k}")).collect();
        let smisses: Vec<String> = misses.iter().map(|k| format!("key-{k}")).collect();
        let tag = match size {
            1000 => "1k",
            100_000 => "100k",
            _ => "1m",
        };
        bench_int::<HashMap<i64, i64>>(&format!("int_{tag}_secure"), &keys, &misses, reps);
        bench_int::<FastIntMap>(&format!("int_{tag}_fast"), &keys, &misses, reps);
        bench_string::<HashMap<&str, i64>>(&format!("str_{tag}_secure"), &skeys, &smisses, reps);
        bench_string::<FastStrMap>(&format!("str_{tag}_fast"), &skeys, &smisses, reps);
    }
    bench_small::<HashMap<i64, i64>>("secure");
    bench_small::<FastIntMap>("fast");
}
