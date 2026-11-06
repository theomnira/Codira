// AURORA: hash maps, the Rust baseline of `src/mod.code`. Kernels are
// named `<op>_<key>_<hasher>_<size>` (op: insert, hit, miss, iter, remove;
// key: i64, string; hasher: fast, secure; size: 1k, 100k, 1m),
// `small_build_<hasher>`, `small_get_<hasher>` and `churn_<hasher>`.
//
// `fast` is `HashMap` with Codira's unkeyed hash functions, exactly: the
// MurmurHash3 finalizer of the integer, FNV-1a 64 of the string's bytes
// (computed by the key's `Hash`, so `str`'s terminator byte is not mixed
// in). `secure` is `HashMap`'s default: SipHash-1-3 under a random key, as
// Codira's `Map.new()`.
#[path = "../common/bench.rs"]
mod bench;

use bench::Rng;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hash, Hasher};
use std::hint::black_box;
use std::time::Instant;

fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51AFD7ED558CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CEB9FE1A85EC53);
    x ^ (x >> 33)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF29CE484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001B3);
    }
    h
}

/// Integers: the finalizer of the integer written. A key that writes a
/// `u64` it already hashed (`FnvKey`) passes through `write_u64` as is.
#[derive(Default)]
struct CodiraHasher(u64);
impl Hasher for CodiraHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn write_i64(&mut self, v: i64) {
        self.0 = mix64(v as u64);
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}
type Fast = BuildHasherDefault<CodiraHasher>;

#[derive(PartialEq, Eq, Clone)]
struct FnvKey(String);
impl Hash for FnvKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u64(fnv1a(self.0.as_bytes()));
    }
}

/// What the kernels need of a key: made from the integer key, and a length
/// for the iteration check.
trait Key: Hash + Eq + Clone {
    fn make(k: i64) -> Self;
    fn weight(&self, k_int: i64) -> i64;
}
impl Key for i64 {
    fn make(k: i64) -> Self {
        k
    }
    fn weight(&self, _: i64) -> i64 {
        *self
    }
}
impl Key for String {
    fn make(k: i64) -> Self {
        format!("key-{k}")
    }
    fn weight(&self, _: i64) -> i64 {
        self.len() as i64
    }
}
impl Key for FnvKey {
    fn make(k: i64) -> Self {
        FnvKey(format!("key-{k}"))
    }
    fn weight(&self, _: i64) -> i64 {
        self.0.len() as i64
    }
}

fn report_ns(ns: u128, check: i64) {
    println!("@@ ns={ns} check={}", check as u64);
    bench::report_rss();
}

/// The value stored for the `i`-th key: `k ^ 5` for integers, `i` for
/// strings, as the Codira program stores them.
fn run<K: Key, S: BuildHasher + Default>(op: &str, n: usize, ints: bool) {
    let mut r = Rng::new();
    let hits: Vec<i64> = (0..n).map(|_| ((r.next() >> 2) * 2) as i64).collect();
    let miss: Vec<i64> = (0..n).map(|_| ((r.next() >> 2) * 2 + 1) as i64).collect();
    let keys: Vec<K> = black_box(hits.iter().map(|&k| K::make(k)).collect());
    let misses: Vec<K> = black_box(miss.iter().map(|&k| K::make(k)).collect());
    let value = |i: usize| if ints { hits[i] ^ 5 } else { i as i64 };
    let reps = if n <= 1000 { 1000 } else if n <= 100_000 { 10 } else { 1 };
    let fill = |m: &mut HashMap<K, i64, S>| {
        for (i, k) in keys.iter().enumerate() {
            m.insert(k.clone(), value(i));
        }
    };
    let mut m: HashMap<K, i64, S> = HashMap::with_hasher(S::default());
    if op != "insert" {
        fill(&mut m);
    }
    let mut ns = 0u128;
    let mut check = 0i64;
    match op {
        "insert" => {
            for _ in 0..reps {
                let t = Instant::now();
                m = HashMap::with_hasher(S::default());
                fill(&mut m);
                ns += t.elapsed().as_nanos();
                check += m.len() as i64;
            }
        }
        "hit" => {
            let t = Instant::now();
            for _ in 0..reps {
                for k in &keys {
                    if let Some(v) = m.get(k) {
                        check += v & 1;
                    }
                }
            }
            ns = t.elapsed().as_nanos();
        }
        "miss" => {
            let t = Instant::now();
            for _ in 0..reps {
                for k in &misses {
                    if m.contains_key(k) {
                        check += 1;
                    }
                }
            }
            ns = t.elapsed().as_nanos();
        }
        "iter" => {
            let t = Instant::now();
            for _ in 0..reps {
                for (k, v) in &m {
                    check += if ints { (k.weight(0) ^ v) & 7 } else { (k.weight(0) + v) & 7 };
                }
            }
            ns = t.elapsed().as_nanos();
        }
        "remove" => {
            for rep in 0..reps {
                if rep > 0 {
                    fill(&mut m);
                }
                let t = Instant::now();
                for k in &keys {
                    if let Some(v) = m.remove(k) {
                        check += v & 1;
                    }
                }
                ns += t.elapsed().as_nanos();
            }
            check += m.len() as i64;
        }
        _ => bench::unknown(op),
    }
    report_ns(ns, check);
}

fn small<S: BuildHasher + Default>(build: bool) {
    let keys: [i64; 5] = [17, 4242, -9, 123456789, 77];
    let mut check = 0i64;
    if build {
        let t = Instant::now();
        for _ in 0..200_000 {
            let mut m: HashMap<i64, i64, S> = HashMap::with_hasher(S::default());
            for &k in black_box(&keys) {
                m.insert(k, k ^ 5);
            }
            check += m.len() as i64;
        }
        report_ns(t.elapsed().as_nanos(), check);
    } else {
        let mut m: HashMap<i64, i64, S> = HashMap::with_hasher(S::default());
        for &k in &keys {
            m.insert(k, k ^ 5);
        }
        let t = Instant::now();
        for i in 0..black_box(1_000_000i64) {
            let k = keys[(i % 8) as usize % 5] + (i & 8);
            if let Some(v) = m.get(&k) {
                check += v & 3;
            }
        }
        report_ns(t.elapsed().as_nanos(), check);
    }
}

fn churn<S: BuildHasher + Default>() {
    let mut r = Rng::new();
    let mut ring = vec![0i64; 65536];
    let mut m: HashMap<i64, i64, S> = HashMap::with_hasher(S::default());
    for slot in ring.iter_mut() {
        let k = (r.next() >> 1) as i64;
        *slot = k;
        m.insert(k, k);
    }
    let t = Instant::now();
    let mut found = 0i64;
    for step in 0..2_000_000usize {
        let slot = step & 65535;
        m.remove(&ring[slot]);
        let k = (r.next() >> 1) as i64;
        ring[slot] = k;
        m.insert(k, k);
        if let Some(v) = m.get(&ring[(step * 7) & 65535]) {
            found += (v & 1) + 1;
        }
    }
    report_ns(t.elapsed().as_nanos(), found * 1_000_000 + m.len() as i64);
}

fn main() {
    let kernel = bench::kernel();
    let parts: Vec<&str> = kernel.split('_').collect();
    match parts.as_slice() {
        ["small", what, hasher] => match (*what, *hasher) {
            ("build", "fast") => small::<Fast>(true),
            ("build", "secure") => small::<RandomState>(true),
            ("get", "fast") => small::<Fast>(false),
            ("get", "secure") => small::<RandomState>(false),
            _ => bench::unknown(&kernel),
        },
        ["churn", "fast"] => churn::<Fast>(),
        ["churn", "secure"] => churn::<RandomState>(),
        [op, key, hasher, size] => {
            let n = match *size {
                "1k" => 1000,
                "100k" => 100_000,
                "1m" => 1_000_000,
                _ => bench::unknown(&kernel),
            };
            match (*key, *hasher) {
                ("i64", "fast") => run::<i64, Fast>(op, n, true),
                ("i64", "secure") => run::<i64, RandomState>(op, n, true),
                ("string", "fast") => run::<FnvKey, Fast>(op, n, false),
                ("string", "secure") => run::<String, RandomState>(op, n, false),
                _ => bench::unknown(&kernel),
            }
        }
        _ => bench::unknown(&kernel),
    }
}
