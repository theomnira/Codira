//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - End-to-end tests of growable arrays and the collections built on them:
//!   the real `codira` binary builds a package and runs it, and its output is
//!   checked. `random_operation_sequences_match_rust_vec` replays random
//!   sequences of operations against Rust's `Vec` as the oracle.

use std::{path::Path, process::Command};

/// Writes a one-file package whose `src/mod.code` is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_collections")
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

/// Runs `codira run` on the package in `dir` with `extra` arguments.
fn codira_run(dir: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
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
    let output = codira_run(dir.path(), &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "codira run failed:\n{stderr}");
    String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
}

/// Builds and runs `source`, which must panic, and returns its stdout and
/// stderr.
fn run_panics(source: &str) -> (String, String) {
    let dir = package_with(source);
    let output = codira_run(dir.path(), &[]);
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(
        output.status.code(),
        Some(101),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

#[test]
fn push_pop_insert_remove_and_friends() {
    let out = run_ok(
        r#"
public func main() {
    let xs: [i64] = []
    println(xs.len(), " ", xs.is_empty(), " ", xs.pop() == nil, " ", xs.last() == nil)
    for i in 0..10 {
        xs.push(i * i)
    }
    println(xs.len(), " ", xs.capacity() >= 10, " ", xs.last()!)
    for x in xs { print(x, " ") }
    println()
    println("popped ", xs.pop()!, " len ", xs.len())
    xs.insert(0, 100)
    xs.insert(xs.len(), 7)
    println("removed ", xs.remove(3), " len ", xs.len())
    for x in xs { print(x, " ") }
    println()
    xs.clear()
    println(xs.len(), " ", xs.is_empty(), " ", xs.capacity() >= 10)
    xs.reserve(1000)
    println(xs.capacity() >= 1000, " ", xs.len())
}
"#,
    );
    assert_eq!(
        out,
        "0 true true true\n\
         10 true 81\n\
         0 1 4 9 16 25 36 49 64 81 \n\
         popped 81 len 9\n\
         removed 4 len 10\n\
         100 0 1 9 16 25 36 49 64 7 \n\
         0 true true\n\
         true 0\n"
    );
}

#[test]
fn searching_sorting_slicing_and_appending() {
    let out = run_ok(
        r#"
public func main() {
    let xs = [5, -3, 9, 0, 9, -12, 4]
    println(xs.contains(9), " ", xs.contains(8), " ", xs.index_of(9)!, " ", xs.index_of(8) == nil)
    xs.sort()
    for x in xs { print(x, " ") }
    println()
    println(xs.binary_search(4)!, " ", xs.binary_search(-12)!, " ", xs.binary_search(1) == nil)
    xs.reverse()
    for x in xs { print(x, " ") }
    println()
    let middle = xs[1..4]
    let tail = xs[5..=6]
    println(middle.len(), " ", middle[0], " ", middle[2], " ", tail.len(), " ", tail[1])
    let empty = xs[3..3]
    println(empty.len())
    middle[0] = 1000
    println(xs[1], " ", middle[0])
    let copy = xs.copy()
    copy.push(1)
    println(xs.len(), " ", copy.len())
    xs.append(middle)
    xs.append(xs)
    println(xs.len(), " ", xs[7], " ", xs[12])
    let words = ["pear", "apple", "fig", "apple"]
    words.sort()
    for w in words { print(w, " ") }
    println()
    println(words.contains("fig"), " ", words.index_of("pear")!, " ", words.binary_search("fig")!)
    let fs = [2.5, -1.0, 0.25]
    fs.sort()
    println(fs[0], " ", fs[2], " ", fs.sum(), " ", fs.min()!, " ", fs.max()!)
    let bs = [true, false, true]
    bs.sort()
    println(bs[0], " ", bs[2], " ", bs.contains(false))
}
"#,
    );
    assert_eq!(
        out,
        "true false 2 true\n\
         -12 -3 0 4 5 9 9 \n\
         3 0 true\n\
         9 9 5 4 0 -3 -12 \n\
         3 9 4 2 -12\n\
         0\n\
         9 1000\n\
         7 8\n\
         20 1000 5\n\
         apple apple fig pear \n\
         true 3 2\n\
         -1.0 2.5 1.75 -1.0 2.5\n\
         false true true\n"
    );
}

#[test]
fn map_filter_fold_and_the_reductions() {
    let out = run_ok(
        r#"
func square(x: i64) -> i64 { x * x }

class Counter { n: i64 }

public func main() {
    let xs = [3, 1, 4, 1, 5, 9, 2, 6]
    let offset = 10
    let shifted = xs.map(func(x) { x + offset })
    let squares = xs.map(square)
    let big = xs.filter(func(x) { x > 3 })
    let total = xs.fold(0, func(acc, x) { acc + x })
    let product = xs.fold(1, func(acc: i64, x: i64) -> i64 { acc * x })
    println(shifted[0], " ", shifted.len(), " ", squares[5], " ", big.len(), " ", big[0])
    println(total, " ", product, " ", xs.sum(), " ", xs.min()!, " ", xs.max()!)
    println(xs.any(func(x) { x == 9 }), " ", xs.any(func(x) { x > 9 }))
    println(xs.all(func(x) { x > 0 }), " ", xs.all(func(x) { x > 1 }))
    let empty: [i64] = []
    println(empty.sum(), " ", empty.min() == nil, " ", empty.any(func(x) { true }), " ", empty.all(func(x) { false }))

    // A closure value (not a literal) is called through.
    let f = func(x: i64) -> bool { x % 2 == 0 }
    println(xs.filter(f).len())

    // A closure that stops early: `any` does not look past the first hit.
    let seen = Counter { n: 0 }
    let hit = xs.any(func(x) { seen.n += 1; x == 4 })
    println(hit, " ", seen.n)

    // `map` to another element type, and to arrays.
    let halves = xs.map(func(x) { x as f64 / 2.0 })
    let flags = xs.map(func(x) { x > 2 })
    let rows = xs.map(func(x) { [x, x] })
    println(halves[0], " ", flags[1], " ", rows[2][1], " ", rows.len())

    // `for (i, x) in xs.enumerate()`.
    var weighted = 0
    for (i, x) in xs.enumerate() {
        weighted += i as i64 * x
    }
    println(weighted)

    // A `return` inside the closure leaves only the closure.
    let clipped = xs.map(func(x: i64) -> i64 { if x > 4 { return 4 } x })
    println(clipped[5], " ", clipped[0])
}
"#,
    );
    assert_eq!(
        out,
        "13 8 81 4 4\n\
         31 6480 31 1 9\n\
         true false\n\
         true false\n\
         0 true false true\n\
         3\n\
         true 3\n\
         1.5 false 4 8\n\
         131\n\
         4 3\n"
    );
}

/// Arrays of classes, structs, strings and arrays grow; generic code pushes.
#[test]
fn elements_of_every_kind_grow() {
    let out = run_ok(
        r#"
struct Point { x: i64, y: i64 }
class Node { value: i64, next: [Node] }

func collect[T](a: T, b: T, n: i64) -> [T] {
    let out: [T] = []
    for i in 0..n {
        if i % 2 == 0 { out.push(a) } else { out.push(b) }
    }
    out
}

public func main() {
    let points: [Point] = []
    let nodes: [Node] = []
    let names: [str] = []
    let grid: [[i64]] = []
    for i in 0..1000 {
        points.push(Point { x: i, y: -i })
        nodes.push(Node { value: i, next: [] })
        names.push(if i % 3 == 0 { "fizz" } else { "buzz" })
        grid.push([i])
        grid[0].push(i)
    }
    for i in 1..1000 {
        nodes[i - 1].next.push(nodes[i])
    }
    println(points[999].y, " ", nodes[500].next[0].value, " ", names[999], " ", grid[0].len(), " ", grid[7][0])
    let removed = points.remove(0)
    println(removed.x, " ", points[0].x, " ", points.len())
    let xs = collect(1.5, 2.5, 5)
    let ss = collect("a", "b", 3)
    println(xs.len(), " ", xs[1], " ", ss[2])
}
"#,
    );
    assert_eq!(out, "-999 501 fizz 1001 7\n0 1 999\n5 2.5 a\n");
}

/// Changing an array while a loop over it runs: pushed elements are not
/// visited, and the loop ends at the new end when the array shrinks.
#[test]
fn loops_over_a_changing_array_stay_in_bounds() {
    let out = run_ok(
        r#"
public func main() {
    let xs = [1, 2, 3]
    var visited = 0
    for x in xs {
        xs.push(x * 10)
        visited += 1
    }
    println(visited, " ", xs.len())

    let ys = [1, 2, 3, 4, 5, 6]
    var sum = 0
    for y in ys {
        sum += y
        ys.pop();
    }
    println(sum, " ", ys.len())

    let zs = [1, 2, 3, 4]
    let doubled = zs.map(func(z) { zs.pop(); z * 2 })
    println(doubled.len(), " ", doubled[1], " ", zs.len())
}
"#,
    );
    assert_eq!(out, "3 6\n6 3\n2 4 2\n");
}

#[test]
fn remove_out_of_range_panics_with_its_location() {
    let (stdout, stderr) = run_panics(
        r#"
public func main() {
    let xs = [1, 2, 3]
    println(xs.remove(1))
    println(xs.remove(2))
}
"#,
    );
    assert_eq!(stdout, "2\n");
    assert!(
        stderr.contains(
            "panic at mod.code:5:13: index out of bounds: the length is 2 but the index is 2"
        ),
        "stderr: {stderr}"
    );
}

#[test]
fn insert_past_the_end_panics_with_its_location() {
    let (_, stderr) = run_panics(
        r#"
public func main() {
    let xs: [i64] = []
    xs.insert(0, 1)
    xs.insert(-1, 2)
}
"#,
    );
    assert!(
        stderr.contains(
            "panic at mod.code:5:5: index out of bounds: the length is 1 but the index is -1"
        ),
        "stderr: {stderr}"
    );
}

#[test]
fn a_slice_out_of_range_panics_with_its_location() {
    let (_, stderr) = run_panics(
        r#"
public func main() {
    let xs = [1, 2, 3]
    let ok = xs[0..3]
    println(ok.len())
    let bad = xs[2..4]
}
"#,
    );
    assert!(
        stderr.contains(
            "panic at mod.code:6:15: index out of bounds: the length is 3 but the index is 4"
        ),
        "stderr: {stderr}"
    );
    let (_, stderr) = run_panics(
        r#"
public func main() {
    let xs = [1, 2, 3]
    let bad = xs[2..1]
}
"#,
    );
    assert!(
        stderr.contains("panic at mod.code:4:15: slice starts after its end"),
        "stderr: {stderr}"
    );
}

/// The optimized IR (`-O 2`) of the package whose `src/mod.code` is
/// `source`.
fn optimized_ir(source: &str) -> String {
    let dir = package_with(source);
    let output = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("build")
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .args(["--emit-ir", "-O", "2", "--color", "disable"])
        .output()
        .expect("failed to spawn codira");
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

/// A closure literal passed to `fold`, `any` or `map` is compiled in place:
/// at `-O 2` the loop has no call and no allocation of a function value,
/// and the `fold` sum vectorizes like the `for` loop written by hand. A
/// function value passed instead is called through.
#[test]
fn closure_literals_are_compiled_in_place() {
    let ir = optimized_ir(
        r#"
public func fold_sum(xs: [i64]) -> i64 { xs.fold(0, func(acc, x) { acc + x }) }
public func loop_sum(xs: [i64]) -> i64 {
    var total: i64 = 0
    for x in xs { total += x }
    total
}
public func any_big(xs: [i64], limit: i64) -> bool { xs.any(func(x) { x > limit }) }
public func doubled(xs: [i64]) -> [i64] { xs.map(func(x) { x * 2 }) }
public func through_value(xs: [i64], f: func(i64, i64) -> i64) -> i64 { xs.fold(0, f) }
"#,
    );
    for name in ["fold_sum", "loop_sum"] {
        let body = function_ir(&ir, name);
        assert!(
            body.contains("llvm.vector.reduce.add") && !body.contains("call ptr"),
            "`{name}` is not a vectorized loop:\n{body}"
        );
        assert!(!body.contains("fn_value"), "`{name}`:\n{body}");
    }
    let any = function_ir(&ir, "any_big");
    assert!(
        !any.contains("call ") && !any.contains("fn_value"),
        "`any_big` calls something:\n{any}"
    );
    // One allocation: the result array, not a function value.
    let map = function_ir(&ir, "doubled");
    assert_eq!(map.matches("tail call ptr").count(), 1, "`doubled`:\n{map}");
    assert!(!map.contains("fn_value"), "`doubled`:\n{map}");
    assert!(
        function_ir(&ir, "through_value").contains("fn_value.call"),
        "a function value is called through"
    );
}

/// A deterministic generator for the property test (SplitMix64).
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
}

/// Generates a random sequence of array operations, writes it as a Codira
/// program that prints the state after every step, and computes the same
/// output with `Vec<i64>`.
fn random_program(seed: u64, steps: usize) -> (String, String) {
    let mut rng = Rng(seed);
    let mut v: Vec<i64> = Vec::new();
    let mut code =
        String::from("public func main() {\n    let xs: [i64] = []\n    var o: i64? = nil\n");
    let mut expected = String::new();
    for _ in 0..steps {
        let value = rng.below(200) as i64 - 100;
        let op = rng.below(100);
        match op {
            0..=34 => {
                code.push_str(&format!("    xs.push({value})\n"));
                v.push(value);
            }
            35..=49 => {
                code.push_str("    o = xs.pop()\n    if let p = o { print(\"p\", p) } else { print(\"p-\") }\n");
                match v.pop() {
                    Some(p) => expected.push_str(&format!("p{p}")),
                    None => expected.push_str("p-"),
                }
            }
            50..=59 => {
                let index = rng.below(v.len() as u64 + 1);
                code.push_str(&format!("    xs.insert({index}, {value})\n"));
                v.insert(index as usize, value);
            }
            60..=69 if !v.is_empty() => {
                let index = rng.below(v.len() as u64);
                code.push_str(&format!("    print(\"r\", xs.remove({index}))\n"));
                expected.push_str(&format!("r{}", v.remove(index as usize)));
            }
            70..=73 => {
                code.push_str("    xs.sort()\n");
                v.sort_unstable();
            }
            74..=77 => {
                code.push_str("    xs.reverse()\n");
                v.reverse();
            }
            78..=81 if !v.is_empty() => {
                let lo = rng.below(v.len() as u64);
                let hi = lo + rng.below(v.len() as u64 - lo + 1);
                code.push_str(&format!("    xs.append(xs[{lo}..{hi}])\n"));
                let copy = v[lo as usize..hi as usize].to_vec();
                v.extend(copy);
            }
            82..=83 => {
                code.push_str("    xs.clear()\n");
                v.clear();
            }
            84..=89 => {
                code.push_str(&format!(
                    "    print(\"c\", xs.contains({value}), \"i\")\n    if let i = xs.index_of({value}) {{ print(i) }} else {{ print(\"-\") }}\n"
                ));
                let index = v.iter().position(|&x| x == value);
                expected.push_str(&format!("c{}i", v.contains(&value)));
                match index {
                    Some(i) => expected.push_str(&i.to_string()),
                    None => expected.push('-'),
                }
            }
            90..=94 => {
                let threshold = value;
                code.push_str(&format!(
                    "    print(\"f\", xs.filter(func(x) {{ x > {threshold} }}).len(), \"m\", xs.map(func(x) {{ x * 3 }}).sum(), \"s\", xs.fold(0, func(a, x) {{ a + x }}))\n"
                ));
                let kept = v.iter().filter(|&&x| x > threshold).count();
                let mapped: i64 = v.iter().map(|x| x * 3).sum();
                let folded: i64 = v.iter().sum();
                expected.push_str(&format!("f{kept}m{mapped}s{folded}"));
            }
            _ => {
                code.push_str("    if let m = xs.min() { print(\"n\", m, \"x\", xs.max()!) } else { print(\"n-\") }\n");
                match (v.iter().min(), v.iter().max()) {
                    (Some(min), Some(max)) => expected.push_str(&format!("n{min}x{max}")),
                    _ => expected.push_str("n-"),
                }
            }
        }
        code.push_str("    print(\" \", xs.len(), \";\")\n");
        expected.push_str(&format!(" {};", v.len()));
    }
    code.push_str("    for x in xs { print(x, \",\") }\n    println()\n}\n");
    for x in &v {
        expected.push_str(&format!("{x},"));
    }
    expected.push('\n');
    (code, expected)
}

#[test]
fn random_operation_sequences_match_rust_vec() {
    for seed in [1, 2, 3, 0xdead_beef, 42] {
        let (code, expected) = random_program(seed, 400);
        let out = run_ok(&code);
        assert_eq!(out, expected, "seed {seed}; program:\n{code}");
    }
}
