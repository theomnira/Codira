//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 20, 2026
//!
//! Functionality:
//! - End-to-end checks of the performance builtins `select`, `likely`,
//!   `unlikely`, `fma` and `array_filled`: the LLVM IR they compile to (a plain
//!   `select`, `llvm.expect`, `llvm.fma`, a fill loop, four independent
//!   accumulator chains, a vectorized integer reduction) and the values the
//!   compiled code computes.

use std::{ffi::OsString, path::Path};

use codira::run_with_args;
use codira_runtime::Runtime;

const SOURCE: &str = r#"
public func pick(c: bool, a: i64, b: i64) -> i64 { select(c, a, b) }
public func pick_f(c: bool, a: f64, b: f64) -> f64 { select(c, a, b) }
public func pick_b(c: bool, a: bool, b: bool) -> bool { select(c, a, b) }
public func fused(a: f64, b: f64, c: f64) -> f64 { fma(a, b, c) }
public func fused32(a: f32, b: f32, c: f32) -> f32 { fma(a, b, c) }

public func rare(x: i64) -> i64 {
    if unlikely(x < 0) { println(x); 0 } else { x }
}
public func usual(x: i64) -> i64 {
    if likely(x >= 0) { x } else { println(x); 0 }
}

public func sum_ints(xs: [i64]) -> i64 {
    var total = 0
    for i in 0..xs.len() { total += xs.unchecked_get(i) }
    total
}

public func dot4(xs: [f64], ys: [f64]) -> f64 {
    var a0 = 0.0
    var a1 = 0.0
    var a2 = 0.0
    var a3 = 0.0
    var i = 0
    let n = xs.len()
    while i + 4 <= n {
        a0 = fma(xs.unchecked_get(i), ys.unchecked_get(i), a0)
        a1 = fma(xs.unchecked_get(i + 1), ys.unchecked_get(i + 1), a1)
        a2 = fma(xs.unchecked_get(i + 2), ys.unchecked_get(i + 2), a2)
        a3 = fma(xs.unchecked_get(i + 3), ys.unchecked_get(i + 3), a3)
        i += 4
    }
    while i < n {
        a0 = fma(xs.unchecked_get(i), ys.unchecked_get(i), a0)
        i += 1
    }
    (a0 + a1) + (a2 + a3)
}

public func dot1_from(xs: [f64], ys: [f64], init: f64) -> f64 {
    var a0 = init
    for i in 0..xs.len() { a0 = fma(xs.unchecked_get(i), ys.unchecked_get(i), a0) }
    a0
}

public func dot4_from(xs: [f64], ys: [f64], init: f64) -> f64 {
    var a0 = init
    var a1 = 0.0
    var a2 = 0.0
    var a3 = 0.0
    var i = 0
    let n = xs.len()
    while i + 4 <= n {
        a0 = fma(xs.unchecked_get(i), ys.unchecked_get(i), a0)
        a1 = fma(xs.unchecked_get(i + 1), ys.unchecked_get(i + 1), a1)
        a2 = fma(xs.unchecked_get(i + 2), ys.unchecked_get(i + 2), a2)
        a3 = fma(xs.unchecked_get(i + 3), ys.unchecked_get(i + 3), a3)
        i += 4
    }
    while i < n {
        a0 = fma(xs.unchecked_get(i), ys.unchecked_get(i), a0)
        i += 1
    }
    (a0 + a1) + (a2 + a3)
}

// `reps` dot products over the same two arrays; each starts from the previous
// result (scaled to stay finite) so the compiler cannot hoist one out.
public func bench_dot1(n: usize, reps: usize) -> f64 {
    let xs = array_filled(0.5, n)
    let ys = array_filled(2.0, n)
    var acc = 0.0
    for r in 0..reps { acc = dot1_from(xs, ys, acc * 0.000001) }
    acc
}
public func bench_dot4(n: usize, reps: usize) -> f64 {
    let xs = array_filled(0.5, n)
    let ys = array_filled(2.0, n)
    var acc = 0.0
    for r in 0..reps { acc = dot4_from(xs, ys, acc * 0.000001) }
    acc
}

public func filled_sum(n: usize, v: i64) -> i64 { sum_ints(array_filled(v, n)) }
public func filled_len(n: usize) -> usize { array_filled(0.5, n).len() }
public func filled_flags(n: usize) -> usize {
    let flags = array_filled(true, n)
    var seen = 0
    for i in 0..flags.len() { if flags.unchecked_get(i) { seen += 1 } }
    seen
}
public func dot_filled(n: usize, x: f64, y: f64) -> f64 {
    dot4(array_filled(x, n), array_filled(y, n))
}
"#;

fn write_package(dir: &Path) {
    std::fs::write(
        dir.join("codira.toml"),
        "[package]\nname=\"perf\"\nauthors=[]\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/mod.code"), SOURCE).unwrap();
}

fn build(dir: &Path, extra: &[&str]) {
    let mut args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        dir.join("codira.toml").into(),
    ];
    args.extend(extra.iter().map(OsString::from));
    assert_eq!(run_with_args(args).unwrap(), codira::ExitStatus::Success);
}

/// The options that give an x86 target its fused multiply-add (an `AArch64`
/// target has one already).
fn fma_options() -> Vec<&'static str> {
    if cfg!(target_arch = "x86_64") {
        vec!["--target-feature", "+fma"]
    } else {
        vec![]
    }
}

/// The IR of the package built with `extra` options.
fn ir_with(extra: &[&str]) -> String {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path());
    let mut options = vec!["--emit-ir"];
    options.extend(fma_options());
    options.extend_from_slice(extra);
    build(project.path(), &options);
    std::fs::read_to_string(project.path().join("target/mod.ll")).unwrap()
}

/// The text of the IR function definition whose name is exactly `name`.
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
fn select_is_a_plain_select_without_a_branch_or_the_unpredictable_promise() {
    for opt in ["0", "2", "3"] {
        let ir = ir_with(&["-O", opt]);
        for (name, ty) in [("pick", "i64"), ("pick_f", "double"), ("pick_b", "i1")] {
            let body = function_ir(&ir, name);
            assert!(
                body.contains("select i1 %") && body.contains(ty),
                "-O{opt}: `{name}` has no select:\n{body}"
            );
            assert!(
                !body.contains("!unpredictable"),
                "-O{opt}: `{name}` promised unpredictability:\n{body}"
            );
            assert!(
                !body.contains("br i1"),
                "-O{opt}: `{name}` branches:\n{body}"
            );
        }
    }
}

#[test]
fn fma_is_the_llvm_fused_intrinsic_at_both_widths() {
    for opt in ["0", "2"] {
        let ir = ir_with(&["-O", opt]);
        assert!(
            function_ir(&ir, "fused").contains("@llvm.fma.f64"),
            "-O{opt}:\n{}",
            function_ir(&ir, "fused")
        );
        assert!(
            function_ir(&ir, "fused32").contains("@llvm.fma.f32"),
            "-O{opt}:\n{}",
            function_ir(&ir, "fused32")
        );
        // Not an `fmul` followed by an `fadd`, which rounds twice.
        for name in ["fused", "fused32"] {
            let body = function_ir(&ir, name);
            assert!(
                !body.contains("fmul") && !body.contains("fadd"),
                "-O{opt}: `{name}` rounds twice:\n{body}"
            );
        }
    }
}

#[test]
fn branch_hints_reach_the_optimizer_as_expect_and_then_branch_weights() {
    let ir = ir_with(&["-O", "0"]);
    for (name, expected) in [("rare", "i1 false"), ("usual", "i1 true")] {
        let body = function_ir(&ir, name);
        assert!(
            body.contains("@llvm.expect.i1(i1 %") && body.contains(expected),
            "-O0: `{name}` lacks `llvm.expect` with {expected}:\n{body}"
        );
    }
    let ir = ir_with(&["-O", "2"]);
    for name in ["rare", "usual"] {
        let body = function_ir(&ir, name);
        assert!(
            body.contains("!prof"),
            "-O2: `{name}` lost its branch weights:\n{body}"
        );
    }
    // The weights point the right way: `rare`'s likely edge is the
    // else-branch, `usual`'s the then-branch.
    let weights = |name: &str| -> (u64, u64) {
        let body = function_ir(&ir, name);
        let branch = body
            .lines()
            // (not a safepoint poll's branch, which has weights of its own)
            .find(|l| l.contains("br i1") && l.contains("!prof") && !l.contains("gc.poll"))
            .unwrap();
        let id = branch
            .rsplit("!prof !")
            .next()
            .unwrap()
            .trim()
            .trim_end_matches(',');
        let id: String = id.chars().take_while(char::is_ascii_digit).collect();
        let node = ir
            .lines()
            .find(|l| l.starts_with(&format!("!{id} = ")))
            .unwrap_or_else(|| panic!("no metadata !{id} in:\n{ir}"));
        let numbers: Vec<u64> = node
            .split("i32 ")
            .skip(1)
            .map(|n| {
                n.split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        (numbers[0], numbers[1])
    };
    let (rare_then, rare_else) = weights("rare");
    assert!(rare_then < rare_else, "rare: {rare_then} vs {rare_else}");
    let (usual_then, usual_else) = weights("usual");
    assert!(
        usual_then > usual_else,
        "usual: {usual_then} vs {usual_else}"
    );
}

#[test]
fn array_filled_is_a_counted_fill_loop() {
    let ir = ir_with(&["-O", "0"]);
    let body = function_ir(&ir, "filled_len");
    for block in ["fill.cond", "fill.body", "fill.done"] {
        assert!(body.contains(block), "no `{block}` in:\n{body}");
    }
    assert!(body.contains("phi i64"), "no counter in:\n{body}");
}

#[test]
fn four_accumulators_stay_four_independent_fma_chains() {
    let ir = ir_with(&["-O", "2"]);
    let body = function_ir(&ir, "dot4");
    let chains = body.matches("call double @llvm.fma.f64").count();
    assert!(chains >= 4, "{chains} fma calls in:\n{body}");
    // One loop-carried `double` per accumulator in the main loop: four chains
    // for the scheduler to overlap rather than one long dependency.
    let carried = body.matches("= phi double").count();
    assert!(carried >= 4, "{carried} carried accumulators in:\n{body}");
}

#[test]
fn an_integer_reduction_is_vectorized_at_o3_and_a_float_one_is_not_reassociated() {
    let ir = ir_with(&["-O", "3"]);
    let body = function_ir(&ir, "sum_ints");
    assert!(
        body.contains("x i64>"),
        "`sum_ints` was not vectorized at -O3:\n{body}"
    );
    // A float sum may not be reassociated without fast-math, so nothing in
    // `dot4` becomes a vector reduction.
    let dot = function_ir(&ir, "dot4");
    assert!(
        !dot.contains("@llvm.vector.reduce.fadd"),
        "`dot4` reassociated a float sum:\n{dot}"
    );
}

/// Without a fused multiply-add the build is refused with the remedy named,
/// instead of failing in the linker on libm's `fma` or rounding twice.
#[cfg(target_arch = "x86_64")]
#[test]
fn fma_without_hardware_support_is_refused() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path());
    let args: Vec<OsString> = vec![
        "codira".into(),
        "build".into(),
        "--manifest-path".into(),
        project.path().join("codira.toml").into(),
    ];
    let error = format!("{:#}", run_with_args(args).unwrap_err());
    assert!(
        error.contains("`fma` needs a target with fused multiply-add"),
        "{error}"
    );
    assert!(error.contains("--target-feature +fma"), "{error}");
}

#[test]
fn the_builtins_compute_the_right_values() {
    let project = tempfile::tempdir().unwrap();
    write_package(project.path());
    #[cfg(target_arch = "x86_64")]
    if !std::arch::is_x86_feature_detected!("fma") {
        eprintln!("skipped: this CPU has no FMA");
        return;
    }
    build(project.path(), &fma_options());
    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();

    for c in [true, false] {
        let got: i64 = runtime.invoke("pick", (c, 7i64, -3i64)).unwrap();
        assert_eq!(got, if c { 7 } else { -3 });
        let got: f64 = runtime.invoke("pick_f", (c, 1.25f64, -8.5f64)).unwrap();
        assert_eq!(got, if c { 1.25 } else { -8.5 });
        let got: bool = runtime.invoke("pick_b", (c, true, false)).unwrap();
        assert_eq!(got, c);
    }

    // One rounding: (1 + 2^-30)^2 - (1 + 2^-29) is exactly 2^-60 fused, and 0
    // if the product is rounded first.
    let e = 2f64.powi(-30);
    let (a, c) = (1.0 + e, -(1.0 + 2.0 * e));
    assert_eq!(a * a + c, 0.0);
    let got: f64 = runtime.invoke("fused", (a, a, c)).unwrap();
    assert_eq!(got, 2f64.powi(-60));
    assert_eq!(got, a.mul_add(a, c));
    let (a32, c32) = (1.0f32 + 2f32.powi(-12), -(1.0f32 + 2.0 * 2f32.powi(-12)));
    let got: f32 = runtime.invoke("fused32", (a32, a32, c32)).unwrap();
    assert_eq!(got, 2f32.powi(-24));

    let got: i64 = runtime.invoke("usual", (9i64,)).unwrap();
    assert_eq!(got, 9);
    let got: i64 = runtime.invoke("rare", (9i64,)).unwrap();
    assert_eq!(got, 9);

    for n in [0usize, 1, 3, 4, 5, 17, 1000] {
        let got: i64 = runtime.invoke("filled_sum", (n, 3i64)).unwrap();
        assert_eq!(got, 3 * n as i64, "n = {n}");
        let got: usize = runtime.invoke("filled_len", (n,)).unwrap();
        assert_eq!(got, n);
        let got: usize = runtime.invoke("filled_flags", (n,)).unwrap();
        assert_eq!(got, n);
        // 0.5 * 4.0 is exact, so any grouping of the sum gives the same bits.
        let got: f64 = runtime.invoke("dot_filled", (n, 0.5f64, 4.0f64)).unwrap();
        assert_eq!(got, 2.0 * n as f64, "n = {n}");
    }
}

/// One accumulator against four, timed. Ignored by default (it measures, it
/// does not assert a speed-up): run it with `cargo test -p codira --test
/// perf_builtins -- --ignored --nocapture`. It prints the minimum
/// and median of 25 interleaved runs of each.
#[test]
#[ignore = "measures; run by hand"]
fn measure_one_accumulator_against_four() {
    #[cfg(target_arch = "x86_64")]
    if !std::arch::is_x86_feature_detected!("fma") {
        return;
    }
    let project = tempfile::tempdir().unwrap();
    write_package(project.path());
    let mut options = fma_options();
    options.extend(["-O", "3"]);
    build(project.path(), &options);
    let library = project.path().join("target/mod.codiralib");
    // Safety: the library was compiled by this test.
    let runtime = unsafe { Runtime::builder(&library).finish() }.unwrap();
    let (n, reps) = (4096usize, 20_000usize);
    let time = |name: &str| {
        let start = std::time::Instant::now();
        let result: f64 = runtime.invoke(name, (n, reps)).unwrap();
        (start.elapsed().as_secs_f64(), result)
    };
    // Same arithmetic, so the same sum up to the order of the additions.
    let (_, one) = time("bench_dot1");
    let (_, four) = time("bench_dot4");
    assert!(
        (one - four).abs() <= 1e-9 * one.abs().max(1.0),
        "{one} vs {four}"
    );
    let (mut ones, mut fours) = (vec![], vec![]);
    for _ in 0..25 {
        ones.push(time("bench_dot1").0);
        fours.push(time("bench_dot4").0);
    }
    for (name, runs) in [("1 accumulator", &mut ones), ("4 accumulators", &mut fours)] {
        runs.sort_by(f64::total_cmp);
        println!(
            "{name}: min {:.2} ms, median {:.2} ms over {} runs ({} fma each)",
            runs[0] * 1e3,
            runs[runs.len() / 2] * 1e3,
            runs.len(),
            n * reps
        );
    }
}
