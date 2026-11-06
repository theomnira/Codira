// AURORA: hash maps, the Rust baseline of `src/mod.code`. The plain
// kernels use `HashMap` with Codira's hash functions, exactly: the
// MurmurHash3 finalizer of the integer, FNV-1a 64 of the string's bytes
// (computed by the key's `Hash`, so `str`'s terminator byte is not mixed
// in). The `*_secure` kernels use `HashMap`'s default: SipHash-1-3 with a
// random seed, which Codira's `Map` has no counterpart of.
#[path = "../common/bench.rs"]
mod bench;

use bench::{report, Rng};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::hint::black_box;

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
type Same = BuildHasherDefault<CodiraHasher>;

#[derive(PartialEq, Eq, Clone)]
struct FnvKey(String);
impl Hash for FnvKey {
    fn hash<H: Hasher>(&self, h: &mut H) {
        h.write_u64(fnv1a(self.0.as_bytes()));
    }
}

fn int_keys() -> Vec<i64> {
    let mut r = Rng::new();
    black_box((0..1_000_000).map(|_| (r.next() >> 1) as i64).collect())
}

fn string_keys() -> Vec<String> {
    let mut r = Rng::new();
    black_box((0..200_000).map(|_| format!("user:{}", r.next() % 100_000_000)).collect())
}

fn build_ints<S: std::hash::BuildHasher + Default>(keys: &[i64]) -> HashMap<i64, i64, S> {
    let mut m = HashMap::with_hasher(S::default());
    for &k in keys {
        m.insert(k, k ^ 5);
    }
    m
}

fn lookup_ints<S: std::hash::BuildHasher + Default>(keys: &[i64]) {
    let m: HashMap<i64, i64, S> = build_ints(keys);
    let start = bench::now();
    let mut found = 0u64;
    for k in keys {
        if let Some(v) = m.get(k) {
            found += (v & 1) as u64 + 1;
        }
    }
    report(start, found);
}

fn lookup_strings<K: Hash + Eq + Clone, S: std::hash::BuildHasher + Default>(keys: &[K]) {
    let mut m: HashMap<K, i64, S> = HashMap::with_hasher(S::default());
    for (i, k) in keys.iter().enumerate() {
        m.insert(k.clone(), i as i64);
    }
    let start = bench::now();
    let mut found = 0u64;
    for _ in 0..5 {
        for k in keys {
            if let Some(v) = m.get(k) {
                found += (v & 7) as u64 + 1;
            }
        }
    }
    report(start, found);
}

fn insert_strings<K: Hash + Eq + Clone, S: std::hash::BuildHasher + Default>(keys: &[K]) {
    let start = bench::now();
    let mut m: HashMap<K, i64, S> = HashMap::with_hasher(S::default());
    for (i, k) in keys.iter().enumerate() {
        m.insert(k.clone(), i as i64);
    }
    report(start, black_box(m).len() as u64);
}

fn churn<S: std::hash::BuildHasher + Default>() {
    let mut r = Rng::new();
    let mut ring = vec![0i64; 65536];
    let mut m: HashMap<i64, i64, S> = HashMap::with_hasher(S::default());
    for slot in ring.iter_mut() {
        let k = (r.next() >> 1) as i64;
        *slot = k;
        m.insert(k, k);
    }
    let start = bench::now();
    let mut found = 0u64;
    for step in 0..2_000_000usize {
        let slot = step & 65535;
        m.remove(&ring[slot]);
        let k = (r.next() >> 1) as i64;
        ring[slot] = k;
        m.insert(k, k);
        if let Some(v) = m.get(&ring[(step * 7) & 65535]) {
            found += (v & 1) as u64 + 1;
        }
    }
    report(start, found * 1_000_000 + m.len() as u64);
}

type Secure = std::collections::hash_map::RandomState;

fn main() {
    let kernel = bench::kernel();
    match kernel.as_str() {
        "insert_i64" | "insert_i64_secure" => {
            let keys = int_keys();
            let start = bench::now();
            let n = if kernel == "insert_i64" {
                black_box(build_ints::<Same>(&keys)).len()
            } else {
                black_box(build_ints::<Secure>(&keys)).len()
            };
            report(start, n as u64);
        }
        "lookup_i64" => lookup_ints::<Same>(&int_keys()),
        "lookup_i64_secure" => lookup_ints::<Secure>(&int_keys()),
        "insert_string" => {
            let keys: Vec<FnvKey> = string_keys().into_iter().map(FnvKey).collect();
            insert_strings::<FnvKey, Same>(&keys)
        }
        "insert_string_secure" => insert_strings::<String, Secure>(&string_keys()),
        "lookup_string" => {
            let keys: Vec<FnvKey> = string_keys().into_iter().map(FnvKey).collect();
            lookup_strings::<FnvKey, Same>(&keys)
        }
        "lookup_string_secure" => lookup_strings::<String, Secure>(&string_keys()),
        "churn_i64" => churn::<Same>(),
        "churn_i64_secure" => churn::<Secure>(),
        k => bench::unknown(k),
    }
}
