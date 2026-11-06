//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - End-to-end tests of lazy pipelines (`xs.iter().map(f).filter(g).sum()`):
//!   the real `codira` binary builds a package and runs it, and its output is
//!   checked.
//! - `random_pipelines_match_rust_iterators` generates random pipelines over
//!   random inputs, whose closures log every element they see, and checks
//!   both the results and the logs against Rust's iterators: the same
//!   values, and the same calls in the same order (interleaving, early
//!   exit).
//! - `pipelines_compile_to_one_loop` checks the optimized IR: no call, no
//!   allocation, no function value in the fused loop.

use std::{cell::RefCell, fmt::Write as _, path::Path, process::Command, rc::Rc};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_pipelines")
        .tempdir()
        .unwrap();
    std::fs::write(
        dir.path().join("codira.toml"),
        "[package]\nname = \"app\"\nauthors = []\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.code"), source).unwrap();
    dir
}

/// Runs `codira <command>` on the package in `dir` with `extra` arguments.
fn codira(command: &str, dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg(command)
        .arg("--manifest-path")
        .arg(dir.join("codira.toml"))
        .arg("--color")
        .arg("disable")
        .args(extra)
        .output()
        .expect("failed to spawn codira")
}

/// Builds and runs `source`, which must succeed, and returns its output.
fn run_ok(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("run", dir.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "codira run failed:\n{stderr}\nprogram:\n{source}"
    );
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

/// Builds `source`, which must fail to compile, and returns the errors.
fn build_fails(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("build", dir.path(), &[]);
    assert!(!output.status.success(), "the build succeeded:\n{source}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The optimized (`-O 2`) IR of `source`.
fn optimized_ir(source: &str) -> String {
    let dir = package_with(source);
    let output = codira("build", dir.path(), &["--emit-ir", "-O", "2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(dir.path().join("target/mod.ll")).unwrap()
}

/// The text of the IR function definition named `name`.
fn function_ir<'a>(ir: &'a str, name: &str) -> &'a str {
    let start = ir
        .match_indices("define ")
        .map(|(start, _)| start)
        .find(|&start| {
            ir[start..]
                .lines()
                .next()
                .is_some_and(|line| line.contains(&format!("@{name}(")))
        })
        .unwrap_or_else(|| panic!("no `{name}` in:\n{ir}"));
    let end = ir[start..].find("\n}").map_or(ir.len(), |e| start + e + 2);
    &ir[start..end]
}

#[test]
fn adapters_and_terminals() {
    let out = run_ok(
        r#"
public func main() {
    let xs: [i64] = [3, 1, 4, 1, 5, 9, 2, 6, 5, 3]
    let ws: [f64] = [1.5, -2.0, 4.25]
    println(xs.iter().map(func(x) { x * 2 }).filter(func(x) { x % 3 == 0 }).sum())
    println(xs.iter().take(4).sum(), " ", xs.iter().skip(7).sum(), " ", xs.iter().skip(20).count())
    println(xs.iter().take_while(func(x) { x < 5 }).count(), " ", xs.iter().skip_while(func(x) { x < 5 }).count())
    println(xs.iter().enumerate().filter(func(p) { p.0 % 2 == 0 }).map(func(p) { p.1 }).sum())
    println(xs.iter().zip(ws).map(func(p) { p.0 as f64 * p.1 }).sum())
    println(xs.iter().zip(10..13).map(func(p) { p.0 * p.1 }).sum())
    println(xs.iter().chain(0..3).count(), " ", xs.iter().take(2).chain(xs.iter().rev().take(2)).sum())
    println(xs.iter().rev().fold(0, func(acc, x) { acc * 10 + x }))
    println(xs.iter().find(func(x) { x > 4 })!, " ", xs.iter().position(func(x) { x > 4 })!)
    println(xs.iter().find(func(x) { x > 40 }) == nil, " ", xs.iter().position(func(x) { x > 40 }) == nil)
    println(xs.iter().min()!, " ", xs.iter().max()!, " ", ws.iter().min()!, " ", ws.iter().max()!)
    println(xs.iter().any(func(x) { x == 9 }), " ", xs.iter().all(func(x) { x > 0 }), " ", xs.iter().all(func(x) { x > 1 }))
    let ys = xs.iter().filter(func(x) { x > 2 }).map(func(x) { x * 100 }).collect()
    for y in ys { print(y, " ") }
    println(ys.len())
    println((0..10).sum(), " ", (1..=10).count(), " ", (1..5).rev().fold(0, func(a, x) { a * 10 + x }))
    println((1..=4).iter().rev().map(func(x) { x * x }).fold(0, func(a, x) { a * 100 + x }))
    println((0..1000000).filter(func(x) { x % 7 == 3 }).take(3).fold(0, func(a, x) { a * 1000 + x }))
    println((250u8..=255u8).count(), " ", (250u8..=255u8).rev().map(func(x) { x as i64 }).sum())
    println((0u8..=255u8).count(), " ", (-128i8..=127i8).rev().count())
    println((5..3).count(), " ", (5..=3).count(), " ", (5..3).rev().count(), " ", (5..=3).rev().count())
}
"#,
    );
    assert_eq!(
        out,
        "42\n9 14 0\n4 6\n19\n19.5\n89\n13 12\n3562951413\n5 4\ntrue true\n1 9 -2.0 4.25\n\
         true true false\n300 400 500 900 600 500 300 7\n45 10 4321\n16090401\n3010017\n6 1515\n\
         256 256\n0 0 0 0\n"
    );
}

#[test]
fn for_loops_over_pipelines() {
    let out = run_ok(
        r#"
func first_big(xs: [i64], limit: i64) -> i64 {
    for (i, x) in xs.iter().enumerate() {
        if x > limit { return i as i64 }
    }
    -1
}

public func main() {
    let xs: [i64] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    for (i, x) in xs.iter().enumerate().filter(func(p) { p.1 % 3 == 0 }) { print(i, ":", x, " ") }
    println()
    var n = 0
    for x in (0..100).filter(func(x) { x % 7 == 0 }) {
        if x > 50 { break }
        if x == 14 { continue }
        n += x
    }
    println(n)
    for (a, b) in xs.iter().zip((1..=3).iter()) { print(a * b, " ") }
    println()
    for x in xs.iter().take(2).chain(0..2).chain(xs.iter().skip(8)) { print(x, " ") }
    println()
    println(first_big(xs, 6), " ", first_big(xs, 60))
    var total = 0
    for x in (0..4).map(func(i) { (0..i).sum() }) { total = total * 10 + x }
    println(total)
}
"#,
    );
    assert_eq!(out, "2:3 5:6 8:9 \n182\n1 4 9 \n1 2 0 1 9 10 \n6 -1\n13\n");
}

#[test]
fn stages_run_interleaved_and_stop_early() {
    let out = run_ok(
        r#"
class Log { xs: [i64] }

func show(log: Log) {
    for v in log.xs { print(v, " ") }
    println()
    log.xs.clear()
}

public func main() {
    let log = Log { xs: [] }
    let xs: [i64] = [1, 2, 3, 4, 5, 6]
    // Each element through both stages before the next; `take(2)` stops
    // pulling after the second element it passes.
    let total = xs.iter()
        .map(func(x) { log.xs.push(x); x * 10 })
        .filter(func(x) { log.xs.push(-x); x != 20 })
        .take(2)
        .sum()
    println(total)
    show(log)
    // `take(0)` pulls nothing.
    println(xs.iter().map(func(x) { log.xs.push(x); x }).take(0).count())
    show(log)
    // `any` stops at the first match, `take_while` at the first rejected
    // element (which it has seen).
    println(xs.iter().map(func(x) { log.xs.push(x); x }).any(func(x) { x == 3 }))
    show(log)
    println(xs.iter().take_while(func(x) { log.xs.push(x); x < 3 }).count())
    show(log)
    // `zip` pulls its first side first: when the other side is exhausted,
    // one more element has gone through the stages before it.
    println(xs.iter().map(func(x) { log.xs.push(x); x }).zip(0..2).count())
    show(log)
    // The second half of a `chain` starts after the first is exhausted.
    println(xs.iter().take(2).map(func(x) { log.xs.push(x); x })
        .chain(xs.iter().map(func(x) { log.xs.push(100 + x); x }))
        .take(3)
        .sum())
    show(log)
    // The eager `map`/`filter` are unchanged: each builds its array first.
    println(xs.map(func(x) { log.xs.push(x); x }).filter(func(x) { log.xs.push(-x); x > 4 }).len())
    show(log)
}
"#,
    );
    assert_eq!(
        out,
        "40\n1 -10 2 -20 3 -30 \n0\n\ntrue\n1 2 3 \n2\n1 2 3 \n2\n1 2 3 \n4\n1 2 101 \n2\n\
         1 2 3 4 5 6 -1 -2 -3 -4 -5 -6 \n"
    );
}

#[test]
fn iterator_objects() {
    let out = run_ok(
        r#"
class Countdown { n: i64 }
extend Countdown {
    func next(self) -> i64? {
        if self.n == 0 { return nil }
        self.n -= 1
        self.n
    }
}

class Cycle[T] { items: [T], at: usize, left: i64 }
extend Cycle[T] {
    func next(self) -> T? {
        if self.left == 0 { return nil }
        self.left -= 1
        let item = self.items[self.at]
        self.at = (self.at + 1) % self.items.len()
        item
    }
}

class Fib { a: i64, b: i64 }
extend Fib {
    func next(self) -> i64? {
        let r = self.a
        self.a = self.b
        self.b = r + self.b
        r
    }
}

public func main() {
    let four = Countdown { n: 4 }
    for x in four.iter() { print(x, " ") }
    println()
    let c = Countdown { n: 5 }
    for x in c { print(x, " ") }
    println(c.n)
    let words = Cycle { items: ["a", "b", "c"], at: 0, left: 7 }
    for w in words { print(w) }
    println()
    let floats = Cycle { items: [0.5, 1.5], at: 0, left: 5 }
    println(floats.iter().sum())
    // An endless iterator, ended by `take` / `take_while` / `find`.
    let f1 = Fib { a: 0, b: 1 }
    println(f1.iter().take(10).fold(0, func(acc, x) { acc * 0 + x }))
    let f2 = Fib { a: 0, b: 1 }
    println(f2.iter().take_while(func(x) { x < 100 }).count())
    let f3 = Fib { a: 0, b: 1 }
    println(f3.iter().find(func(x) { x > 1000 })!)
    let fib = Fib { a: 0, b: 1 }
    println(fib.iter().enumerate().map(func(p) { p.0 as i64 * p.1 }).take(5).sum(), " ", fib.a)
    let three = Countdown { n: 3 }
    println((0..3).chain(three.iter()).collect().len())
}
"#,
    );
    assert_eq!(
        out,
        "3 2 1 0 \n4 3 2 1 0 0\nabcabca\n4.5\n34\n12\n1597\n21 5\n6\n"
    );
}

#[test]
fn collect_keeps_references_alive_and_arrays_change_safely() {
    let out = run_ok(
        r#"
class Node { v: i64, pad: [i64] }

public func main() {
    // Every element allocates, so collections run while `collect` holds the
    // only references to the nodes made so far.
    let nodes = (0..20000).map(func(i) { Node { v: i, pad: array_filled(i, 8 as usize) } }).collect()
    var sum = 0
    for n in nodes { sum += n.v + n.pad[7] }
    println(nodes.len(), " ", sum)
    let words = (0..5).map(func(i) { f"w{i}" }).collect()
    for w in words { print(w, " ") }
    println()
    // A closure that grows the source: the pipeline still visits only the
    // elements that were there when it started.
    let xs: [i64] = [1, 2, 3]
    println(xs.iter().map(func(x) { xs.push(x); x }).count(), " ", xs.len())
    // One that shortens it: the pipeline ends at the new end, forwards and
    // backwards.
    let ys: [i64] = [1, 2, 3, 4, 5, 6]
    println(ys.iter().map(func(x) { ys.pop(); x }).collect().len(), " ", ys.len())
    let zs: [i64] = [1, 2, 3, 4, 5, 6]
    println(zs.iter().rev().map(func(x) { zs.pop(); zs.pop(); x }).sum(), " ", zs.len())
}
"#,
    );
    assert_eq!(out, "20000 399980000\nw0 w1 w2 w3 w4 \n3 6\n3 3\n6 4\n");
}

#[test]
fn pipelines_in_generic_functions_and_with_function_values() {
    let out = run_ok(
        r#"
func count_if[T](xs: [T], keep: func(T) -> bool) -> usize { xs.iter().filter(keep).count() }
func firsts[A, B](pairs: [(A, B)]) -> [A] { pairs.iter().map(func(p) { p.0 }).collect() }
func square(x: i64) -> i64 { x * x }
func returns_early(xs: [i64]) -> i64 {
    xs.iter().map(func(x) -> i64 { if x > 2 { return 0 } x }).sum()
}

public func main() {
    let xs: [i64] = [1, 2, 3, 4]
    println(count_if(xs, func(x) { x % 2 == 0 }), " ", count_if(["a", "bb"], func(s) { s.len() > 1 }))
    let fs = firsts([(1.5, true), (2.5, false)])
    println(fs[0] + fs[1])
    println(xs.iter().map(square).sum(), " ", returns_early(xs))
    let add = func(a: i64, b: i64) -> i64 { a + b }
    println(xs.iter().fold(0, add))
}
"#,
    );
    assert_eq!(out, "2 1\n4.0\n30 3\n10\n");
}

#[test]
fn misuse_is_a_compile_error() {
    let errors = build_fails(
        r#"
public func main() {
    let xs: [i64] = [1, 2, 3]
    let p = xs.iter().map(func(x) { x + 1 })
    println(xs.iter().enumerate().rev().count())
}
"#,
    );
    assert!(
        errors.contains(
            "a lazy pipeline (`Iter[i64]`) exists only inside the expression that builds it"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("`rev()` walks its source from the back"),
        "{errors}"
    );
}

#[test]
fn the_jit_runs_pipelines_on_llvm() {
    let dir = package_with(
        r#"
public func main() {
    println((0..10).map(func(i) { i * i }).sum())
}
"#,
    );
    let output = codira("run", dir.path(), &["--jit"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("running on the LLVM backend")
            && stderr.contains("a lazy pipeline (`iter()`, `map`, `sum`, ...) (in `main`)"),
        "{stderr}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "285\n");
}

/// A fused pipeline with closure literals is one loop at `-O 2`: no call
/// (other than LLVM's own intrinsics), no allocation and no function value.
/// The eager chain it replaces allocates an array per stage.
#[test]
fn pipelines_compile_to_one_loop() {
    let ir = optimized_ir(
        r#"
public func chain_sum(xs: [i64]) -> i64 {
    xs.iter().map(func(x) { x * 2 }).filter(func(x) { x % 3 == 0 }).sum()
}
public func eager_sum(xs: [i64]) -> i64 {
    xs.map(func(x) { x * 2 }).filter(func(x) { x % 3 == 0 }).sum()
}
public func enum_count(xs: [i64]) -> usize {
    xs.iter().enumerate().filter(func(p) { (p.0 as i64 + p.1) % 3 == 0 }).count()
}
public func dot(xs: [f64], ys: [f64]) -> f64 {
    xs.iter().zip(ys).fold(0.0, func(acc, p) { acc + p.0 * p.1 })
}
public func prefix(xs: [i64], limit: i64) -> i64 {
    xs.iter().take_while(func(x) { x < limit }).sum()
}
public func range_sum(n: i64) -> i64 {
    (0..n).map(func(i) { i * 3 }).filter(func(i) { i % 2 == 0 }).sum()
}
public func searches(xs: [i64], k: i64) -> i64 {
    let at = if let i = xs.iter().rev().position(func(x) { x == k }) { i as i64 } else { -1 }
    let m = if let v = xs.iter().skip(1).take(5).max() { v } else { 0 }
    at + m
}
public func looped(xs: [i64], ys: [i64]) -> i64 {
    var t: i64 = 0
    for (a, b) in xs.iter().chain(ys).enumerate() { t += a as i64 * b }
    t
}
public func doubled(xs: [i64]) -> [i64] {
    xs.iter().map(func(x) { x * 2 }).collect()
}
"#,
    );
    for name in [
        "chain_sum",
        "enum_count",
        "dot",
        "prefix",
        "range_sum",
        "searches",
        "looped",
    ] {
        let body = function_ir(&ir, name);
        let calls: Vec<&str> = body
            .lines()
            .filter(|line| line.contains("call ") && !line.contains("@llvm."))
            .collect();
        assert!(calls.is_empty(), "`{name}` calls something:\n{body}");
        assert!(
            !body.contains("dispatchTable") && !body.contains("fn_value"),
            "`{name}` allocates or makes a function value:\n{body}"
        );
        assert!(!body.contains("#closure"), "`{name}`:\n{body}");
    }
    // The same chain, eager: two arrays.
    let eager = function_ir(&ir, "eager_sum");
    let allocations = |body: &str| {
        body.lines()
            .filter(|line| line.contains("call ptr %new_array"))
            .count()
    };
    assert_eq!(allocations(eager), 2, "{eager}");
    // `collect` allocates its result (with the exact length) and nothing
    // else.
    let doubled = function_ir(&ir, "doubled");
    assert_eq!(allocations(doubled), 1, "{doubled}");
    assert!(!doubled.contains("fn_value"), "{doubled}");
}

// ---------------------------------------------------------------------------
// Random pipelines against Rust's iterators.

/// A deterministic generator (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A value in `lo..=hi`.
    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    fn array(&mut self) -> Vec<i64> {
        let len = self.below(26) as usize;
        (0..len).map(|_| self.int(-20, 20)).collect()
    }
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Array,
    /// `lo..hi`, and whether it is written `(lo..hi).iter()`.
    Range(i64, i64, bool),
    RangeInclusive(i64, i64),
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    /// `x * k + c`.
    Map(i64, i64),
    /// `x % m == r`.
    Filter(i64, i64),
    Take(usize),
    Skip(usize),
    /// `x < t`.
    TakeWhile(i64),
    /// `x < t`.
    SkipWhile(i64),
    /// `enumerate`, then `i * a + x`.
    Enumerate(i64),
    /// `zip` with the second array, then `a - b`.
    ZipArray,
    /// `zip` with `lo..hi`, then `a + b`.
    ZipRange(i64, i64),
    ChainArray,
    ChainRange(i64, i64),
    /// `chain` with the second array mapped by `y + c`.
    ChainMapped(i64),
    Rev,
}

#[derive(Clone, Copy, Debug)]
enum Terminal {
    Sum,
    Count,
    Fold,
    /// `x > t`.
    Any(i64),
    /// `x > t`.
    All(i64),
    /// `x % m == r`.
    Find(i64, i64),
    /// `x > t`.
    Position(i64),
    Min,
    Max,
    Collect,
    /// A `for` loop that breaks at the first element above `t`.
    For(i64),
}

#[derive(Debug)]
struct Pipeline {
    xs: Vec<i64>,
    ys: Vec<i64>,
    source: Source,
    stages: Vec<Stage>,
    terminal: Terminal,
    /// Whether the closures leave the log alone: then the pipeline only
    /// computes, which the code generator compiles differently (its arrays
    /// are read without checking their current length).
    quiet: bool,
}

fn random_pipeline(rng: &mut Rng) -> Pipeline {
    let source = match rng.below(4) {
        0 | 1 => Source::Array,
        2 => {
            let lo = rng.int(-10, 10);
            Source::Range(lo, lo + rng.int(-2, 25), rng.below(2) == 0)
        }
        _ => {
            let lo = rng.int(-10, 10);
            Source::RangeInclusive(lo, lo + rng.int(-2, 25))
        }
    };
    let mut stages = Vec::new();
    // `rev` only while nothing but `map` and `filter` came before it.
    let mut reversible = true;
    for _ in 0..rng.below(7) {
        let stage = match rng.below(13) {
            0 | 1 => Stage::Map(rng.int(-3, 3), rng.int(-10, 10)),
            2 | 3 => {
                let m = rng.int(2, 4);
                Stage::Filter(m, rng.int(-(m - 1), m - 1))
            }
            4 => Stage::Take(rng.below(12) as usize),
            5 => Stage::Skip(rng.below(12) as usize),
            6 => Stage::TakeWhile(rng.int(-15, 25)),
            7 => Stage::SkipWhile(rng.int(-15, 25)),
            8 => Stage::Enumerate(rng.int(-2, 2)),
            9 => {
                if rng.below(2) == 0 {
                    Stage::ZipArray
                } else {
                    let lo = rng.int(-5, 5);
                    Stage::ZipRange(lo, lo + rng.int(0, 20))
                }
            }
            10 => match rng.below(3) {
                0 => Stage::ChainArray,
                1 => {
                    let lo = rng.int(-5, 5);
                    Stage::ChainRange(lo, lo + rng.int(-1, 8))
                }
                _ => Stage::ChainMapped(rng.int(-5, 5)),
            },
            _ if reversible => Stage::Rev,
            _ => Stage::Map(1, rng.int(-3, 3)),
        };
        reversible &= matches!(stage, Stage::Map(..) | Stage::Filter(..) | Stage::Rev);
        stages.push(stage);
    }
    let terminal = match rng.below(11) {
        0 => Terminal::Sum,
        1 => Terminal::Count,
        2 => Terminal::Fold,
        3 => Terminal::Any(rng.int(-10, 30)),
        4 => Terminal::All(rng.int(-30, 10)),
        5 => {
            let m = rng.int(2, 5);
            Terminal::Find(m, rng.int(-(m - 1), m - 1))
        }
        6 => Terminal::Position(rng.int(-10, 30)),
        7 => Terminal::Min,
        8 => Terminal::Max,
        9 => Terminal::Collect,
        _ => Terminal::For(rng.int(-10, 40)),
    };
    Pipeline {
        xs: rng.array(),
        ys: rng.array(),
        source,
        stages,
        terminal,
        quiet: rng.below(3) == 0,
    }
}

/// A Codira array literal of `i64`s.
fn array_literal(values: &[i64]) -> String {
    let items: Vec<String> = values.iter().map(i64::to_string).collect();
    format!("[{}]", items.join(", "))
}

/// The closure log tag of stage `index` (and of the terminal, `index` 9).
fn tag(index: usize) -> i64 {
    (index as i64 + 1) * 100_000
}

/// Writes pipeline `p` (number `n`) as Codira statements that print its
/// result and then what its closures logged.
fn codira_pipeline(n: usize, p: &Pipeline) -> String {
    let mut code = format!(
        "    let xs{n}: [i64] = {}\n    let ys{n}: [i64] = {}\n",
        array_literal(&p.xs),
        array_literal(&p.ys)
    );
    let mut chain = match p.source {
        Source::Array => format!("xs{n}.iter()"),
        Source::Range(lo, hi, false) => format!("({lo}i64..{hi}i64)"),
        Source::Range(lo, hi, true) => format!("({lo}i64..{hi}i64).iter()"),
        Source::RangeInclusive(lo, hi) => format!("({lo}i64..={hi}i64)"),
    };
    for (index, stage) in p.stages.iter().enumerate() {
        let tag = tag(index);
        let log = if p.quiet {
            String::new()
        } else {
            format!("log.xs.push({tag} + x); ")
        };
        let step = match *stage {
            Stage::Map(k, c) => format!(".map(func(x) {{ {log}x * ({k}) + ({c}) }})"),
            Stage::Filter(m, r) => format!(".filter(func(x) {{ {log}x % {m} == ({r}) }})"),
            Stage::Take(k) => format!(".take({k})"),
            Stage::Skip(k) => format!(".skip({k})"),
            Stage::TakeWhile(t) => format!(".take_while(func(x) {{ {log}x < ({t}) }})"),
            Stage::SkipWhile(t) => format!(".skip_while(func(x) {{ {log}x < ({t}) }})"),
            Stage::Enumerate(a) => {
                format!(".enumerate().map(func(p) {{ p.0 as i64 * ({a}) + p.1 }})")
            }
            Stage::ZipArray => format!(".zip(ys{n}).map(func(p) {{ p.0 - p.1 }})"),
            Stage::ZipRange(lo, hi) => {
                format!(".zip(({lo}i64)..({hi}i64)).map(func(p) {{ p.0 + p.1 }})")
            }
            Stage::ChainArray => format!(".chain(ys{n})"),
            Stage::ChainRange(lo, hi) => format!(".chain(({lo})..({hi}))"),
            Stage::ChainMapped(c) => format!(
                ".chain(ys{n}.iter().map(func(y) {{ {}y + ({c}) }}))",
                log.replace("+ x)", "+ y)")
            ),
            Stage::Rev => ".rev()".to_string(),
        };
        chain.push_str(&step);
    }
    let log = if p.quiet {
        String::new()
    } else {
        format!("log.xs.push({} + x); ", tag(9))
    };
    match p.terminal {
        Terminal::Sum => writeln!(code, "    println({chain}.sum())").unwrap(),
        Terminal::Count => writeln!(code, "    println({chain}.count())").unwrap(),
        Terminal::Fold => writeln!(
            code,
            "    println({chain}.fold(7, func(a, x) {{ {log}(a * 31 + x) % 1000003 }}))"
        )
        .unwrap(),
        Terminal::Any(t) => {
            writeln!(code, "    println({chain}.any(func(x) {{ {log}x > ({t}) }}))").unwrap();
        }
        Terminal::All(t) => {
            writeln!(code, "    println({chain}.all(func(x) {{ {log}x > ({t}) }}))").unwrap();
        }
        Terminal::Find(m, r) => writeln!(
            code,
            "    if let v = {chain}.find(func(x) {{ {log}x % {m} == ({r}) }}) {{ println(v) }} else {{ println(\"nil\") }}"
        )
        .unwrap(),
        Terminal::Position(t) => writeln!(
            code,
            "    if let v = {chain}.position(func(x) {{ {log}x > ({t}) }}) {{ println(v) }} else {{ println(\"nil\") }}"
        )
        .unwrap(),
        Terminal::Min | Terminal::Max => {
            let op = if matches!(p.terminal, Terminal::Min) { "min" } else { "max" };
            writeln!(
                code,
                "    if let v = {chain}.{op}() {{ println(v) }} else {{ println(\"nil\") }}"
            )
            .unwrap();
        }
        Terminal::Collect => writeln!(
            code,
            "    let c{n} = {chain}.collect()\n    for v in c{n} {{ print(v, \" \") }}\n    println(c{n}.len())"
        )
        .unwrap(),
        Terminal::For(t) => writeln!(
            code,
            "    var acc{n}: i64 = 0\n    for x in {chain} {{\n        {log}\n        if x > ({t}) {{ break }}\n        acc{n} = (acc{n} * 3 + x) % 1000003\n    }}\n    println(acc{n})"
        )
        .unwrap(),
    }
    code.push_str("    show(log)\n");
    code
}

type Log = Rc<RefCell<Vec<i64>>>;

/// An iterator under construction: double-ended while only `map`, `filter`
/// and `rev` have been applied, so that `rev` is Rust's own.
enum Iter<'a> {
    Both(Box<dyn DoubleEndedIterator<Item = i64> + 'a>),
    Forward(Box<dyn Iterator<Item = i64> + 'a>),
}

impl<'a> Iter<'a> {
    fn forward(self) -> Box<dyn Iterator<Item = i64> + 'a> {
        match self {
            Iter::Both(it) => it,
            Iter::Forward(it) => it,
        }
    }
}

/// Runs pipeline `p` with Rust's iterators: what the Codira program prints
/// for it.
fn rust_pipeline(p: &Pipeline) -> String {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let quiet = p.quiet;
    let mut it = match p.source {
        Source::Array => Iter::Both(Box::new(p.xs.clone().into_iter())),
        Source::Range(lo, hi, _) => Iter::Both(Box::new(lo..hi)),
        Source::RangeInclusive(lo, hi) => Iter::Both(Box::new(lo..=hi)),
    };
    for (index, stage) in p.stages.iter().enumerate() {
        let tag = tag(index);
        let log = Rc::clone(&log);
        it = match (*stage, it) {
            (Stage::Map(k, c), Iter::Both(it)) => Iter::Both(Box::new(it.map(move |x| {
                if !quiet {
                    log.borrow_mut().push(tag + x);
                }
                x.wrapping_mul(k).wrapping_add(c)
            }))),
            (Stage::Map(k, c), it) => Iter::Forward(Box::new(it.forward().map(move |x| {
                if !quiet {
                    log.borrow_mut().push(tag + x);
                }
                x.wrapping_mul(k).wrapping_add(c)
            }))),
            (Stage::Filter(m, r), Iter::Both(it)) => Iter::Both(Box::new(it.filter(move |&x| {
                if !quiet {
                    log.borrow_mut().push(tag + x);
                }
                x % m == r
            }))),
            (Stage::Filter(m, r), it) => Iter::Forward(Box::new(it.forward().filter(move |&x| {
                if !quiet {
                    log.borrow_mut().push(tag + x);
                }
                x % m == r
            }))),
            (Stage::Rev, Iter::Both(it)) => Iter::Both(Box::new(it.rev())),
            (Stage::Rev, Iter::Forward(_)) => unreachable!("the generator checks `rev`"),
            (Stage::Take(k), it) => Iter::Forward(Box::new(it.forward().take(k))),
            (Stage::Skip(k), it) => Iter::Forward(Box::new(it.forward().skip(k))),
            (Stage::TakeWhile(t), it) => {
                Iter::Forward(Box::new(it.forward().take_while(move |&x| {
                    if !quiet {
                        log.borrow_mut().push(tag + x);
                    }
                    x < t
                })))
            }
            (Stage::SkipWhile(t), it) => {
                Iter::Forward(Box::new(it.forward().skip_while(move |&x| {
                    if !quiet {
                        log.borrow_mut().push(tag + x);
                    }
                    x < t
                })))
            }
            (Stage::Enumerate(a), it) => Iter::Forward(Box::new(
                it.forward()
                    .enumerate()
                    .map(move |(i, x)| (i as i64).wrapping_mul(a).wrapping_add(x)),
            )),
            (Stage::ZipArray, it) => Iter::Forward(Box::new(
                it.forward()
                    .zip(p.ys.clone())
                    .map(|(a, b)| a.wrapping_sub(b)),
            )),
            (Stage::ZipRange(lo, hi), it) => Iter::Forward(Box::new(
                it.forward().zip(lo..hi).map(|(a, b)| a.wrapping_add(b)),
            )),
            (Stage::ChainArray, it) => Iter::Forward(Box::new(it.forward().chain(p.ys.clone()))),
            (Stage::ChainRange(lo, hi), it) => Iter::Forward(Box::new(it.forward().chain(lo..hi))),
            (Stage::ChainMapped(c), it) => Iter::Forward(Box::new(it.forward().chain(
                p.ys.clone().into_iter().map(move |y| {
                    if !quiet {
                        log.borrow_mut().push(tag + y);
                    }
                    y.wrapping_add(c)
                }),
            ))),
        };
    }
    let mut it = it.forward();
    let terminal_tag = tag(9);
    let note = |x: i64| {
        if !quiet {
            log.borrow_mut().push(terminal_tag + x);
        }
    };
    let optional = |v: Option<String>| v.unwrap_or_else(|| "nil".to_string());
    let mut out = match p.terminal {
        Terminal::Sum => it.fold(0i64, i64::wrapping_add).to_string(),
        Terminal::Count => it.count().to_string(),
        Terminal::Fold => it
            .fold(7i64, |a, x| {
                note(x);
                a.wrapping_mul(31).wrapping_add(x) % 1_000_003
            })
            .to_string(),
        Terminal::Any(t) => it
            .any(|x| {
                note(x);
                x > t
            })
            .to_string(),
        Terminal::All(t) => it
            .all(|x| {
                note(x);
                x > t
            })
            .to_string(),
        Terminal::Find(m, r) => optional(
            it.find(|&x| {
                note(x);
                x % m == r
            })
            .map(|v| v.to_string()),
        ),
        Terminal::Position(t) => optional(
            it.position(|x| {
                note(x);
                x > t
            })
            .map(|v| v.to_string()),
        ),
        Terminal::Min => optional(it.min().map(|v| v.to_string())),
        Terminal::Max => optional(it.max().map(|v| v.to_string())),
        Terminal::Collect => {
            let items: Vec<i64> = it.collect();
            let mut s = String::new();
            for v in &items {
                write!(s, "{v} ").unwrap();
            }
            write!(s, "{}", items.len()).unwrap();
            s
        }
        Terminal::For(t) => {
            let mut acc = 0i64;
            for x in it.by_ref() {
                note(x);
                if x > t {
                    break;
                }
                acc = (acc.wrapping_mul(3).wrapping_add(x)) % 1_000_003;
            }
            acc.to_string()
        }
    };
    out.push('\n');
    for v in log.borrow().iter() {
        write!(out, "{v} ").unwrap();
    }
    out.push('\n');
    out
}

/// Random pipelines over random arrays and ranges, whose closures log what
/// they see, print the same results and the same logs as the same
/// pipelines written with Rust's iterators.
#[test]
fn random_pipelines_match_rust_iterators() {
    let (mut logged, mut stages, mut early, mut quiet) = (0, 0, 0, 0);
    for seed in [1u64, 2, 3, 0xdead_beef, 42, 7, 1234, 99] {
        let mut rng = Rng(seed);
        let mut code = String::from(
            "class Log { xs: [i64] }\n\nfunc show(log: Log) {\n    for v in log.xs { print(v, \" \") }\n    println()\n    log.xs.clear()\n}\n\npublic func main() {\n    let log = Log { xs: [] }\n",
        );
        let mut expected = String::new();
        let mut pipelines = Vec::new();
        for n in 0..30 {
            let p = random_pipeline(&mut rng);
            code.push_str(&codira_pipeline(n, &p));
            expected.push_str(&rust_pipeline(&p));
            pipelines.push(p);
        }
        code.push_str("}\n");
        let out = run_ok(&code);
        if out != expected {
            let (got, want): (Vec<&str>, Vec<&str>) =
                (out.lines().collect(), expected.lines().collect());
            let line = got
                .iter()
                .zip(&want)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(want.len()));
            panic!(
                "seed {seed}: pipeline {} differs\n  got:  {:?}\n  want: {:?}\n{:#?}\nprogram:\n{code}",
                line / 2,
                got.get(line),
                want.get(line),
                pipelines.get(line / 2)
            );
        }
        logged += expected
            .lines()
            .skip(1)
            .step_by(2)
            .map(|line| line.split_whitespace().count())
            .sum::<usize>();
        stages += pipelines.iter().map(|p| p.stages.len()).sum::<usize>();
        quiet += pipelines.iter().filter(|p| p.quiet).count();
        early += pipelines
            .iter()
            .filter(|p| {
                p.stages
                    .iter()
                    .any(|s| matches!(s, Stage::Take(_) | Stage::TakeWhile(_) | Stage::ZipArray))
                    || matches!(
                        p.terminal,
                        Terminal::Any(_) | Terminal::Find(..) | Terminal::For(_)
                    )
            })
            .count();
    }
    // The generator is not degenerate: the 240 pipelines have stages, stop
    // early, their closures ran, and some only compute.
    assert!(
        stages > 400 && early > 80 && logged > 1500 && quiet > 50,
        "{stages} stages, {early} pipelines that stop early, {logged} closure calls, \
         {quiet} that only compute"
    );
}
