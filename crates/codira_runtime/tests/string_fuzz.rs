//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - The string fuzzing gate: seeded random strings (ASCII, multi-byte
//!   UTF-8, white space of every kind, separators, empty strings) go
//!   through every runtime string operation from Codira code, and Rust's
//!   `str` is the oracle: concatenation, comparison, `find`/`rfind`,
//!   `contains`/`starts_with`/`ends_with`, `split`, `lines`, the trims,
//!   `slice` at char boundaries, ASCII case mapping, `replace`, `repeat`,
//!   `parse_i64`, `parse_f64`, `hash` (FNV-1a), `char_count`, `chars` and
//!   integer `to_string`. The heap is capped small, so collections run in
//!   the middle of the operations.
//!
//! `CODIRA_FUZZ_SECS` (default 3) and `CODIRA_FUZZ_SEED` (default: from the
//! clock, printed) set the run.
//!
//! What it cannot catch: operations whose arguments the generator never
//! produces (slices at non-boundaries and empty `split` separators panic by
//! design and are left out), and float formatting, which follows `print`'s
//! own rules rather than Rust's.
use std::time::{Duration, Instant};

use codira_runtime::{ArrayRef, Runtime, StringRef};
use codira_test::CompileAndRunTestDriver;

const OPS: &str = r#"
    public func concat(a: String, b: String) -> String { a + b }
    public func append(a: String, b: String) -> String { var s = a.to_string(); s += b; s }
    public func compare(a: String, b: String) -> i64 {
        if a < b { -1 } else if a == b { 0 } else { 1 }
    }
    public func find(a: String, b: String) -> i64 { if let i = a.find(b) { i as i64 } else { -1 } }
    public func rfind(a: String, b: String) -> i64 { if let i = a.rfind(b) { i as i64 } else { -1 } }
    public func contains(a: String, b: String) -> i64 {
        var r = 0
        if a.contains(b) { r += 1 }
        if a.starts_with(b) { r += 2 }
        if a.ends_with(b) { r += 4 }
        r
    }
    public func split(a: String, sep: String) -> [String] { a.split(sep) }
    public func lines(a: String) -> [String] { a.lines() }
    public func trims(a: String) -> [String] {
        let out: [String] = []
        out.push(a.trim())
        out.push(a.trim_start())
        out.push(a.trim_end())
        out
    }
    public func slice(a: String, start: usize, end: usize) -> String { a.slice(start, end) }
    public func cases(a: String) -> [String] {
        let out: [String] = []
        out.push(a.to_upper())
        out.push(a.to_lower())
        out
    }
    public func replace(a: String, from: String, to: String) -> String { a.replace(from, to) }
    public func repeat(a: String, n: i64) -> String { a.repeat(n) }
    public func parse_i64(a: String) -> [i64] {
        let out: [i64] = []
        if let v = a.parse_i64() { out.push(v) }
        out
    }
    public func parse_f64(a: String) -> [f64] {
        let out: [f64] = []
        if let v = a.parse_f64() { out.push(v) }
        out
    }
    public func hash(a: String) -> u64 { a.hash() }
    public func char_count(a: String) -> usize { a.char_count() }
    public func chars(a: String) -> [u32] { a.chars() }
    public func int_to_string(v: i64) -> String { v.to_string() }
    public func len(a: String) -> usize { a.len() }
"#;

/// Pieces random strings are made of.
const PIECES: &[&str] = &[
    "a",
    "b",
    "ab",
    "abc",
    "A",
    "Z",
    "z",
    "0",
    "1",
    "-",
    "+",
    ",",
    ", ",
    ";",
    ".",
    "e",
    "E",
    "inf",
    "NaN",
    " ",
    "  ",
    "\t",
    "\n",
    "\r\n",
    "\r",
    "\u{a0}",
    "\u{2003}",
    "\u{3000}",
    "\u{85}",
    "é",
    "É",
    "ß",
    "日本",
    "語",
    "🦀",
    "👍🏽",
    "\u{301}",
    "\u{0}",
    "\u{7f}",
    "x\u{feff}",
    "ǅ",
    "İ",
    "ﬁ",
    "Σ",
    "σς",
    "123",
    "-9223372036854775808",
    "9223372036854775807",
    "9223372036854775808",
    "1e10",
    "0.5",
    "-0.0",
    "1_000",
    "0x10",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn string(&mut self, max_pieces: usize) -> String {
        (0..self.below(max_pieces + 1))
            .map(|_| PIECES[self.below(PIECES.len())])
            .collect()
    }

    /// A pattern: often a substring of `text`, so searches find things.
    fn pattern(&mut self, text: &str) -> String {
        if !text.is_empty() && self.below(2) == 0 {
            let (a, b) = self.boundaries(text);
            text[a..b].to_owned()
        } else {
            self.string(2)
        }
    }

    /// Two char boundaries of `text`, in order.
    fn boundaries(&mut self, text: &str) -> (usize, usize) {
        let mut cuts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        cuts.push(text.len());
        let a = cuts[self.below(cuts.len())];
        let b = cuts[self.below(cuts.len())];
        (a.min(b), a.max(b))
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn strings(array: ArrayRef<'_, StringRef<'_>>) -> Vec<String> {
    array.iter().map(|s| s.as_str().to_owned()).collect()
}

/// One random operation, checked against Rust.
fn check_one(runtime: &Runtime, rng: &mut Rng) {
    let a = rng.string(6);
    let s = |text: &str| StringRef::new(runtime, text);
    let context = format!("{a:?}");
    match rng.below(17) {
        0 => {
            let b = rng.string(4);
            let got: StringRef<'_> = runtime.invoke("concat", (s(&a), s(&b))).unwrap();
            assert_eq!(got.as_str(), format!("{a}{b}"), "{context} + {b:?}");
            let got: StringRef<'_> = runtime.invoke("append", (s(&a), s(&b))).unwrap();
            assert_eq!(got.as_str(), format!("{a}{b}"), "{context} += {b:?}");
        }
        1 => {
            let b = if rng.below(4) == 0 {
                a.clone()
            } else {
                rng.string(6)
            };
            let got: i64 = runtime.invoke("compare", (s(&a), s(&b))).unwrap();
            let expected = match a.as_bytes().cmp(b.as_bytes()) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            assert_eq!(got, expected, "{context} cmp {b:?}");
        }
        2 => {
            let p = rng.pattern(&a);
            let got: i64 = runtime.invoke("find", (s(&a), s(&p))).unwrap();
            assert_eq!(
                got,
                a.find(&p).map_or(-1, |i| i as i64),
                "{context}.find({p:?})"
            );
            let got: i64 = runtime.invoke("rfind", (s(&a), s(&p))).unwrap();
            assert_eq!(
                got,
                a.rfind(&p).map_or(-1, |i| i as i64),
                "{context}.rfind({p:?})"
            );
        }
        3 => {
            let p = rng.pattern(&a);
            let got: i64 = runtime.invoke("contains", (s(&a), s(&p))).unwrap();
            let expected = i64::from(a.contains(&p))
                + 2 * i64::from(a.starts_with(&p))
                + 4 * i64::from(a.ends_with(&p));
            assert_eq!(got, expected, "{context} contains/starts/ends {p:?}");
        }
        4 => {
            let mut sep = rng.pattern(&a);
            if sep.is_empty() {
                sep.push(',');
            }
            let got: ArrayRef<'_, StringRef<'_>> =
                runtime.invoke("split", (s(&a), s(&sep))).unwrap();
            let expected: Vec<&str> = a.split(sep.as_str()).collect();
            assert_eq!(strings(got), expected, "{context}.split({sep:?})");
        }
        5 => {
            let got: ArrayRef<'_, StringRef<'_>> = runtime.invoke("lines", (s(&a),)).unwrap();
            let expected: Vec<&str> = a.lines().collect();
            assert_eq!(strings(got), expected, "{context}.lines()");
        }
        6 => {
            let got: ArrayRef<'_, StringRef<'_>> = runtime.invoke("trims", (s(&a),)).unwrap();
            assert_eq!(
                strings(got),
                [a.trim(), a.trim_start(), a.trim_end()],
                "{context} trims"
            );
        }
        7 => {
            let (start, end) = rng.boundaries(&a);
            let got: StringRef<'_> = runtime.invoke("slice", (s(&a), start, end)).unwrap();
            assert_eq!(
                got.as_str(),
                &a[start..end],
                "{context}.slice({start}, {end})"
            );
        }
        8 => {
            let got: ArrayRef<'_, StringRef<'_>> = runtime.invoke("cases", (s(&a),)).unwrap();
            assert_eq!(
                strings(got),
                [a.to_ascii_uppercase(), a.to_ascii_lowercase()],
                "{context} cases"
            );
        }
        9 => {
            let from = rng.pattern(&a);
            let to = rng.string(2);
            let got: StringRef<'_> = runtime
                .invoke("replace", (s(&a), s(&from), s(&to)))
                .unwrap();
            assert_eq!(
                got.as_str(),
                a.replace(from.as_str(), &to),
                "{context}.replace({from:?}, {to:?})"
            );
        }
        10 => {
            let n = rng.below(7) as i64 - 2;
            let got: StringRef<'_> = runtime.invoke("repeat", (s(&a), n)).unwrap();
            assert_eq!(
                got.as_str(),
                a.repeat(n.max(0) as usize),
                "{context}.repeat({n})"
            );
        }
        11 => {
            let text = if rng.below(2) == 0 {
                PIECES[rng.below(PIECES.len())].to_owned()
            } else {
                rng.string(2)
            };
            let got: ArrayRef<'_, i64> = runtime.invoke("parse_i64", (s(&text),)).unwrap();
            let got: Vec<i64> = got.iter().collect();
            let expected: Vec<i64> = text.parse::<i64>().ok().into_iter().collect();
            assert_eq!(got, expected, "{text:?}.parse_i64()");
        }
        12 => {
            let text = if rng.below(2) == 0 {
                PIECES[rng.below(PIECES.len())].to_owned()
            } else {
                rng.string(2)
            };
            let got: ArrayRef<'_, f64> = runtime.invoke("parse_f64", (s(&text),)).unwrap();
            let got: Vec<u64> = got.iter().map(f64::to_bits).collect();
            let expected: Vec<u64> = text
                .parse::<f64>()
                .ok()
                .map(f64::to_bits)
                .into_iter()
                .collect();
            // NaN payloads may differ; compare NaN-ness then.
            let nan = |bits: &Vec<u64>| {
                bits.iter()
                    .map(|b| f64::from_bits(*b).is_nan())
                    .collect::<Vec<_>>()
            };
            if nan(&expected).iter().any(|n| *n) {
                assert_eq!(nan(&got), nan(&expected), "{text:?}.parse_f64()");
            } else {
                assert_eq!(got, expected, "{text:?}.parse_f64()");
            }
        }
        13 => {
            let got: u64 = runtime.invoke("hash", (s(&a),)).unwrap();
            assert_eq!(got, fnv1a(a.as_bytes()), "{context}.hash()");
            let got: usize = runtime.invoke("len", (s(&a),)).unwrap();
            assert_eq!(got, a.len(), "{context}.len()");
        }
        14 => {
            let got: usize = runtime.invoke("char_count", (s(&a),)).unwrap();
            assert_eq!(got, a.chars().count(), "{context}.char_count()");
        }
        15 => {
            let got: ArrayRef<'_, u32> = runtime.invoke("chars", (s(&a),)).unwrap();
            let got: Vec<u32> = got.iter().collect();
            let expected: Vec<u32> = a.chars().map(u32::from).collect();
            assert_eq!(got, expected, "{context}.chars()");
        }
        _ => {
            let v = match rng.below(4) {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => 0,
                _ => rng.next() as i64,
            };
            let got: StringRef<'_> = runtime.invoke("int_to_string", (v,)).unwrap();
            assert_eq!(got.as_str(), v.to_string());
        }
    }
}

#[test]
fn string_operations_agree_with_rust() {
    let driver = CompileAndRunTestDriver::new(OPS, |builder| builder.heap_limit(32 * 64 * 1024))
        .expect("Failed to build test driver");
    let seed = std::env::var("CODIRA_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    let secs: f64 = std::env::var("CODIRA_FUZZ_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3.0);
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut rng = Rng(seed);
    let mut cases = 0u64;
    eprintln!("string fuzz: seed {seed}");
    while cases < 100 || Instant::now() < deadline {
        check_one(&driver.runtime, &mut rng);
        cases += 1;
    }
    eprintln!(
        "string fuzz: {cases} operations, {} collections at the heap limit",
        driver.runtime.gc().emergency_collections()
    );
}
