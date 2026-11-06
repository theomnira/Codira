//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of the keyed hashing in `std.hashlib.hash` and the
//!   Swiss-table `Map` of `std.collections.map`, built and run by the real
//!   `codira` binary against this source tree's standard library:
//!   SipHash-1-3 and SipHash-2-4 against a reference implementation (itself
//!   checked against the authors' test vector and Rust's own SipHash),
//!   `SipHasher13`, keys that differ between maps and between runs,
//!   hash-flooding keys that make the unkeyed hash quadratic and leave the
//!   keyed default fast, the documented rules for changing a map while
//!   iterating it, custom key types, the runtime's entropy service, and the
//!   compiler features the map relies on (`@inline` across modules,
//!   `array_filled` of any element type).
//! - Random operation sequences against Rust's `HashMap` are in
//!   `tests/std_imports.rs` (`std_collections_map_matches_rust_hash_map`).

use std::{
    hash::Hasher,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The standard library of this source tree, named explicitly so that the
/// tests never pick up another checkout's.
fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../std")
        .canonicalize()
        .unwrap()
}

/// Writes a package whose `src/` holds `files` (`(name, source)` pairs).
fn package_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_hash_map")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    for (name, source) in files {
        std::fs::write(dir.path().join("src").join(name), source).unwrap();
    }
    dir
}

fn codira(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .args(args)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .env("CODIRA_STD_PATH", std_dir())
        .output()
        .expect("failed to spawn codira")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

/// `codira run` of the package in `dir`; panics unless it succeeds.
fn run_dir(dir: &Path) -> String {
    let output = codira(dir, &["run"]);
    let stdout = text(&output.stdout);
    assert!(
        output.status.success(),
        "codira run failed\nstdout:\n{stdout}\nstderr:\n{}",
        text(&output.stderr)
    );
    stdout
}

/// Builds and runs a one-file program; returns its stdout.
fn run_ok(source: &str) -> String {
    let dir = package_with(&[("mod.code", source)]);
    run_dir(dir.path())
}

/// SipHash-c-d of `data` under `(k0, k1)`: the reference algorithm of
/// Aumasson and Bernstein, written out for the comparison.
fn siphash(c: usize, d: usize, k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let round = |v: &mut [u64; 4]| {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    };
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let m = u64::from_le_bytes(chunk.try_into().unwrap());
        v[3] ^= m;
        for _ in 0..c {
            round(&mut v);
        }
        v[0] ^= m;
    }
    let mut last = (data.len() as u64) << 56;
    for (i, &b) in chunks.remainder().iter().enumerate() {
        last |= u64::from(b) << (8 * i);
    }
    v[3] ^= last;
    for _ in 0..c {
        round(&mut v);
    }
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..d {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

struct Xorshift(u64);
impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The reference above is SipHash: it gives the authors' published vector,
/// Rust's `SipHasher` (2-4) under random keys and Rust's `DefaultHasher`
/// (1-3, the zero key) on every length from 0 to 64.
#[test]
#[allow(deprecated)] // `SipHasher` is the standard library's SipHash-2-4.
fn the_reference_siphash_is_siphash() {
    // The SipHash paper, appendix A: key 00..0f, message 00..0e.
    let key = (0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let message: Vec<u8> = (0..15).collect();
    assert_eq!(siphash(2, 4, key.0, key.1, &message), 0xa129_ca61_49be_45e5);
    let mut r = Xorshift(0x5eed);
    for len in 0..=64 {
        let data: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        let (k0, k1) = (r.next(), r.next());
        let mut std24 = std::hash::SipHasher::new_with_keys(k0, k1);
        std24.write(&data);
        assert_eq!(siphash(2, 4, k0, k1, &data), std24.finish(), "len {len}");
        let mut std13 = std::collections::hash_map::DefaultHasher::new();
        std13.write(&data);
        assert_eq!(siphash(1, 3, 0, 0, &data), std13.finish(), "len {len}");
    }
}

/// `std.hashlib.hash`'s SipHash-1-3 and SipHash-2-4 -- over `[u8]`, `str`,
/// `String` and integers, through the functions and through `hash_keyed` --
/// agree with the reference on random keys and every length from 0 to 40,
/// and `SipHasher13` is SipHash-1-3 of its word encoding.
#[test]
fn std_siphash_matches_the_reference() {
    let mut r = Xorshift(0xC0D1_7A);
    let mut program = String::from(
        "import std.hashlib.hash.{Hashable, HashKey, SipHasher13, siphash13, siphash13_str, \
         siphash13_u64, siphash24_str, siphash24_u64}\n\npublic func main() {\n",
    );
    let mut expected = String::new();
    for len in 0..=40usize {
        // Printable ASCII, so the bytes can be a string literal.
        let data: Vec<u8> = (0..len).map(|_| b'a' + (r.next() % 26) as u8).collect();
        let s = String::from_utf8(data.clone()).unwrap();
        let (k0, k1) = (r.next(), r.next());
        let x = r.next();
        program.push_str(&format!(
            "    let k{len} = HashKey.new({k0}u64, {k1}u64)\n    \
             let b{len}: [u8] = \"{s}\".to_string().bytes()\n    \
             let s{len}: String = \"{s}\".to_string()\n    \
             println(siphash13(k{len}, b{len}), \" \", siphash13_str(k{len}, \"{s}\"), \" \", \
             \"{s}\".hash_keyed(k{len}), \" \", s{len}.hash_keyed(k{len}), \" \", \
             siphash24_str(k{len}, \"{s}\"))\n    \
             println(siphash13_u64(k{len}, {x}u64), \" \", siphash24_u64(k{len}, {x}u64), \" \", \
             {x}u64.hash_keyed(k{len}), \" \", ({x}u64 as i64).hash_keyed(k{len}))\n    \
             println(SipHasher13.new(k{len}).write_u64({x}u64).write_str(\"{s}\").finish())\n"
        ));
        let h13 = siphash(1, 3, k0, k1, &data);
        let h24 = siphash(2, 4, k0, k1, &data);
        expected.push_str(&format!("{h13} {h13} {h13} {h13} {h24}\n"));
        let w13 = siphash(1, 3, k0, k1, &x.to_le_bytes());
        let w24 = siphash(2, 4, k0, k1, &x.to_le_bytes());
        expected.push_str(&format!("{w13} {w24} {w13} {w13}\n"));
        // The encoding: the word, the string's bytes in words (the last
        // padded with zeros), then its length.
        let mut words = x.to_le_bytes().to_vec();
        let mut padded = data.clone();
        padded.resize(data.len().div_ceil(8) * 8, 0);
        words.extend_from_slice(&padded);
        words.extend_from_slice(&(len as u64).to_le_bytes());
        expected.push_str(&format!("{}\n", siphash(1, 3, k0, k1, &words)));
    }
    program.push_str("}\n");
    assert_eq!(run_ok(&program), expected);
}

/// `Map.new()` draws a key of its own from the runtime's entropy service:
/// two maps, and two runs of one program, hash (and so iterate) differently,
/// while `Map.with_key` with a fixed key iterates the same way every time.
#[test]
fn keyed_maps_differ_between_maps_and_between_runs() {
    let program = r#"
import std.collections.map.{Map}
import std.hashlib.hash.{HashKey, Hashable}

func order(m: Map[i64, i64]) -> i64 {
    var h: i64 = 0
    for (k, v) in m.iter() { h = h * 1000003 + k }
    h
}

public func main() {
    let a: Map[i64, i64] = Map.new()
    let b: Map[i64, i64] = Map.new()
    let c: Map[i64, i64] = Map.with_key(HashKey.new(1, 2))
    for i in 0..1000 { a.insert(i, i); b.insert(i, i); c.insert(i, i); }
    println(order(a) != order(b))
    println(order(c))
    let x: i64 = 42
    println(x.hash_keyed(HashKey.random()))
}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let first = run_dir(dir.path());
    let second = run_dir(dir.path());
    let (first, second): (Vec<&str>, Vec<&str>) =
        (first.lines().collect(), second.lines().collect());
    assert_eq!(first[0], "true", "two maps iterated in the same order");
    assert_eq!(first[1], second[1], "a fixed key must iterate the same way");
    assert_ne!(first[2], second[2], "two runs drew the same key");
}

/// The inverse of the MurmurHash3 finalizer `mix64` (the unkeyed integer
/// hash): with it, keys whose unkeyed hashes all land in one group with
/// one tag can be computed, as an attacker would.
fn unmix64(mut x: u64) -> u64 {
    fn inverse(a: u64) -> u64 {
        // Newton's iteration for the inverse of an odd number mod 2^64.
        let mut y = a;
        for _ in 0..6 {
            y = y.wrapping_mul(2u64.wrapping_sub(a.wrapping_mul(y)));
        }
        y
    }
    // `x ^= x >> 33` is its own inverse (33 >= 32).
    x ^= x >> 33;
    x = x.wrapping_mul(inverse(0xC4CE_B9FE_1A85_EC53));
    x ^= x >> 33;
    x = x.wrapping_mul(inverse(0xFF51_AFD7_ED55_8CCD));
    x ^ (x >> 33)
}

fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    x = x.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    x ^ (x >> 33)
}

/// Hash flooding: integer keys chosen so that their unkeyed hashes share
/// the low 24 bits (one group in any table up to 2^24 groups) and the top 7
/// (one tag) make every insertion into an `unkeyed` map probe all earlier
/// keys and compare with each: quadratic. The program computes them as an
/// attacker would, by inverting the unkeyed hash (`mix64`, a bijection).
/// The same keys in a keyed map (the default) are spread by SipHash under a
/// key the attacker never saw, and insert as fast as random keys. Measured
/// inside the program; the margin asked for (20x) is far below the one
/// seen (hundreds of times).
#[test]
fn flooding_keys_slow_the_unkeyed_hash_and_not_the_keyed_default() {
    let n = 8_000u64;
    let flood = |i: u64| 0x5A00_0000_0000_0000 | (i << 24) | 0x00AB_CDEF;
    let odd = (0..n)
        .filter(|&i| {
            let key = unmix64(flood(i));
            assert_eq!(mix64(key), flood(i));
            key & 1 == 1
        })
        .count();
    let program = r#"
import std.collections.map.{Map}
import std.hashlib.hash.{Hashable, mix64}

extern "C" {
    func codira_time_monotonic_ns() -> u64;
}

// The inverse of `mix64`: its two multipliers' inverses mod 2^64, and
// `x ^= x >> 33`, which is its own inverse.
func unmix64(value: u64) -> u64 {
    var x = value
    x = x ^ (x >> 33)
    x = x * 0x9CB4B2F8129337DB
    x = x ^ (x >> 33)
    x = x * 0x4F74430C22A54005
    x ^ (x >> 33)
}

func fill(m: Map[i64, i64], keys: [i64]) -> u64 {
    let t = codira_time_monotonic_ns()
    for k in keys { m.insert(k, k); }
    var found: i64 = 0
    for k in keys { found += m.get_or(k, 0) & 1 }
    let ns = codira_time_monotonic_ns() - t
    println(m.len(), " ", found)
    ns
}

public func main() {
    let keys: [i64] = []
    let random: [i64] = []
    var x: u64 = 88172645463325252
    var i: u64 = 0
    while i < N {
        let key = unmix64(0x5A00000000000000 | (i << 24) | 0x00ABCDEF)
        if i == 0 { println(mix64(key) == 0x5A00000000ABCDEF, " ", (key as i64).hash_value() == mix64(key)) }
        keys.push(key as i64)
        x = x ^ (x << 13)
        x = x ^ (x >> 7)
        x = x ^ (x << 17)
        random.push((x >> 1) as i64)
        i += 1
    }
    let flooded = fill(Map.unkeyed(), keys)
    let keyed = fill(Map.new(), keys)
    let baseline = fill(Map.new(), random)
    println(flooded > 20 * keyed, " ", keyed < 20 * baseline + 1000000)
    println(flooded / 1000, " ", keyed / 1000, " ", baseline / 1000)
}
"#
    .replace("N {", &format!("{n} {{"));
    let stdout = run_ok(&program);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "true true", "{stdout}");
    assert_eq!(lines[1], format!("{n} {odd}"), "{stdout}");
    assert_eq!(lines[2], format!("{n} {odd}"), "{stdout}");
    assert_eq!(
        lines[4], "true true",
        "flooded/keyed/random microseconds: {}",
        lines[5]
    );
}

/// The iteration rules `std.collections.map` documents: removing entries
/// (the current one or others) and replacing values while iterating are
/// allowed -- every entry present throughout is produced exactly once, and
/// one removed before it is reached is not -- and an insertion that grows
/// the table stops the iteration with a panic.
#[test]
fn iteration_follows_the_documented_mutation_rules() {
    let stdout = run_ok(
        r#"
import std.collections.map.{Map}

public func main() {
    let m: Map[i64, i64] = Map.new()
    for i in 0..1000 { m.insert(i, i); }
    // Remove every entry produced, and every odd key not reached yet.
    var produced: i64 = 0
    var sum: i64 = 0
    for (k, v) in m.iter() {
        produced += 1
        sum += v
        m.remove(k);
        if k % 2 == 0 { m.remove(k + 1); }
    }
    println(m.len(), " ", produced <= 1000, " ", produced >= 500)
    // Replace every value while iterating: each key is produced once.
    for i in 0..1000 { m.insert(i, i); }
    var seen: i64 = 0
    for (k, v) in m.iter() {
        m.insert(k, v * 2);
        seen += 1
    }
    var total: i64 = 0
    for (k, v) in m.iter() { total += v }
    println(seen, " ", total)
    // `keys()` is a copy: changing the map while looping over it is safe.
    for k in m.keys() { m.remove(k); }
    println(m.len())
}
"#,
    );
    assert_eq!(stdout, "0 true true\n1000 999000\n0\n");

    let dir = package_with(&[(
        "mod.code",
        r#"
import std.collections.map.{Map}

public func main() {
    let m: Map[i64, i64] = Map.new()
    for i in 0..7 { m.insert(i, i); }
    for (k, v) in m.iter() {
        m.insert(k + 100, v);
    }
}
"#,
    )]);
    let output = codira(dir.path(), &["run"]);
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("the map was resized or cleared while being iterated"),
        "{stderr}"
    );
}

/// A custom hash is the key type's: a wrapper implementing `Hashable` and
/// `Eq` (here, strings equal up to ASCII case), under every hasher; `Set`
/// and `Counter` take the same constructors.
#[test]
fn custom_key_types_bring_their_own_hash() {
    let stdout = run_ok(
        r#"
import std.collections.map.{Map}
import std.collections.set.{Set}
import std.collections.counter.{Counter}
import std.hashlib.hash.{Hashable, HashKey, SipHasher13}
import std.cmp.cmp.{Eq}

struct NoCase { text: String }

extend NoCase: Hashable {
    func hash_value(self) -> u64 { self.text.to_lower().hash() }
    func hash_keyed(self, key: HashKey) -> u64 {
        SipHasher13.new(key).write_string(self.text.to_lower()).finish()
    }
}
extend NoCase: Eq {
    func eq(self, other: NoCase) -> bool { self.text.to_lower() == other.text.to_lower() }
}

func nc(s: str) -> NoCase { NoCase { text: s.to_string() } }

func count(m: Map[NoCase, i64]) -> i64 {
    m.insert(nc("Ada"), 1)
    m.insert(nc("ADA"), 2)
    m.insert(nc("alan"), 3)
    m.get(nc("ada"))! * 10 + m.len() as i64
}

public func main() {
    println(count(Map.new()), " ", count(Map.unkeyed()), " ", count(Map.with_key(HashKey.new(3, 4))))
    let s: Set[i64] = Set.with_capacity(100)
    let u: Set[i64] = Set.unkeyed()
    println(s.insert(5), " ", s.insert(5), " ", u.insert(1))
    let c: Counter[str] = Counter.unkeyed()
    c.add("x"); c.add("x")
    println(c.count("x"))
}
"#,
    );
    assert_eq!(stdout, "22 22 22\ntrue false true\n2\n");
}

/// `std.random.rng`'s entropy functions read the operating system's
/// generator through the runtime: bytes and seeds differ between calls and
/// between runs.
#[test]
fn the_entropy_service_gives_fresh_bytes() {
    let program = r#"
import std.random.rng.{os_random_u64, os_random_bytes, rng_from_entropy, rng_next_u64}

public func main() {
    let a = os_random_u64()!
    let b = os_random_u64()!
    println(a != b)
    let bytes = os_random_bytes(100)!
    var ones: u64 = 0
    for x in bytes { if x > 127 { ones += 1 } }
    println(bytes.len(), " ", ones > 10 && ones < 90)
    println(rng_next_u64(rng_from_entropy()!))
}
"#;
    let dir = package_with(&[("mod.code", program)]);
    let first = run_dir(dir.path());
    let second = run_dir(dir.path());
    let first: Vec<&str> = first.lines().collect();
    let second: Vec<&str> = second.lines().collect();
    assert_eq!(first[0], "true");
    assert_eq!(first[1], "100 true");
    assert_ne!(first[2], second[2]);
}

/// `array_filled` takes any element type: tuples, strings and class
/// references (copies of the one reference), as `Map` fills its slots.
#[test]
fn array_filled_takes_any_element_type() {
    let stdout = run_ok(
        r#"
class Cell { n: i64 }

public func main() {
    let pairs = array_filled((7, "x"), 3)
    let strings = array_filled("ab".to_string(), 2)
    let cell = Cell { n: 1 }
    let cells = array_filled(cell, 4)
    cells[0].n = 5
    println(pairs.len(), " ", pairs[2].0, pairs[2].1, " ", strings[1], " ", cells[3].n)
}
"#,
    );
    assert_eq!(stdout, "3 7x ab 5\n");
}

/// An `@inline` function of another module is compiled into the caller's
/// library -- no call through the dispatch table -- and behaves the same.
#[test]
fn inline_functions_of_other_modules_are_compiled_in() {
    let dir = package_with(&[
        (
            "mod.code",
            r#"
import root.util.{twice, plain}

public func use_them(x: i64) -> i64 {
    twice(x) + plain(x)
}

public func main() {
    println(use_them(5))
}
"#,
        ),
        (
            "util.code",
            r#"
@inline
public func twice(x: i64) -> i64 { helper(x) * 2 }

func helper(x: i64) -> i64 { x + 1 }

public func plain(x: i64) -> i64 { x - 1 }
"#,
        ),
    ]);
    assert_eq!(run_dir(dir.path()), "16\n");
    let output = codira(dir.path(), &["build", "--emit-ir", "-O", "2"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let ir = std::fs::read_to_string(dir.path().join("target/mod.ll")).unwrap();
    assert!(
        !ir.contains("%twice_ptr"),
        "`twice` is called through the table:\n{ir}"
    );
    assert!(
        ir.contains("%plain_ptr"),
        "`plain` should be a call into its library:\n{ir}"
    );
}
