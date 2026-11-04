//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Differential testing of the two code generators. Random, well-typed
//!   programs from `differential_gen` (seeded, free of undefined behaviour by
//!   construction) are run three ways -- the LLVM backend at the default
//!   optimization level (`codira run`, which also runs Eidos), the LLVM
//!   backend at `-O 0`, and the Cranelift backend (`codira run --jit`) -- and
//!   must print the same thing and exit the same way.
//! - A Cranelift run that falls back to LLVM because the program uses
//!   something Cranelift does not compile is a generator failure, not a pass:
//!   it would compare LLVM with itself. A fallback because Cranelift's
//!   verifier rejected the code is a backend divergence.
//! - On a divergence the program is written under the test's temporary
//!   directory (`CARGO_TARGET_TMPDIR/differential/`), reduced by deleting
//!   calls and statements while the same divergence persists, and the test
//!   fails naming the seed and both files.
//! - `CODIRA_DIFF_PROGRAMS` and `CODIRA_DIFF_SEED` change how many programs
//!   run and from which seed; `many_programs` (ignored by default) runs
//!   thousands in parallel.
//! - Known divergences are tested as such, with their explanation, so that a
//!   fix is noticed: see the `known_divergence_*` tests, and
//!   the `known_divergence_*` tests below for the record.
//!
//! Passing is evidence, not proof: it says these programs agree, nothing about
//! programs the generator cannot write.

mod differential_gen;

use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use differential_gen::{generate, Config, Program};

const DEFAULT_SEED: u64 = 0xC0D1_7A00;
const TIMEOUT: Duration = Duration::from_secs(120);

/// One way of running a program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    LlvmDefault,
    LlvmO0,
    Cranelift,
}

impl Backend {
    const ALL: [Backend; 3] = [Backend::LlvmDefault, Backend::LlvmO0, Backend::Cranelift];

    fn name(self) -> &'static str {
        match self {
            Backend::LlvmDefault => "llvm -O 2",
            Backend::LlvmO0 => "llvm -O 0",
            Backend::Cranelift => "cranelift",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Backend::LlvmDefault => &[],
            Backend::LlvmO0 => &["-O", "0"],
            Backend::Cranelift => &["--jit"],
        }
    }
}

/// What a run printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    /// `None` when the process was killed (timeout) or died by a signal.
    exit: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Outcome {
    /// The part that must agree between backends.
    fn observable(&self) -> (Option<i32>, &str) {
        (self.exit, &self.stdout)
    }
}

fn package(source: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("codira_diff")
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

/// Runs `source` on `backend`, in a package of its own so no build cache is
/// shared between backends. Output goes to files, so a program that prints a
/// lot cannot fill a pipe and stall.
fn run(source: &str, backend: Backend) -> Outcome {
    let dir = package(source);
    let out_path = dir.path().join("stdout.txt");
    let err_path = dir.path().join("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_codira"))
        .arg("run")
        .arg("--manifest-path")
        .arg(dir.path().join("codira.toml"))
        .args(["--color", "disable"])
        .args(backend.args())
        .env("CODIRA_JIT_TIMINGS", "1")
        .stdin(Stdio::null())
        .stdout(File::create(&out_path).unwrap())
        .stderr(File::create(&err_path).unwrap())
        .spawn()
        .expect("failed to spawn codira");
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    Outcome {
        exit: status.and_then(|s| s.code()),
        stdout: std::fs::read_to_string(&out_path)
            .unwrap_or_default()
            .replace("\r\n", "\n"),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    }
}

/// Whether a Cranelift run really ran on Cranelift.
fn ran_on_cranelift(outcome: &Outcome) -> bool {
    outcome.stderr.contains("cranelift: compiled in")
}

/// The result of comparing the backends on one program.
enum Verdict {
    /// No difference other than known divergences, which are named.
    Agree { known: Vec<&'static str> },
    /// The generator wrote something the reference backend does not compile,
    /// or something Cranelift declines as unsupported. Not a backend
    /// divergence, but a failure all the same.
    Invalid(String),
    /// The backends disagree. `signature` names the kind of disagreement
    /// (which backend, and a crash site or "different output"), so the
    /// reducer keeps the same bug instead of wandering off to another.
    Diverge { signature: String, report: String },
}

/// A divergence already recorded by a `known_divergence_*` test
/// §12 and pinned by a `known_divergence_*` test, recognised by what the
/// failing backend printed. The match is narrow on purpose (backend, and the
/// exact error text), so a different bug at the same crash site still fails.
fn known_divergence(backend: Backend, outcome: &Outcome) -> Option<&'static str> {
    let e = &outcome.stderr;
    if backend == Backend::LlvmO0
        && e.contains("Instruction does not dominate all uses!")
        && e.contains("phi {}")
    {
        return Some("llvm -O 0: invalid unit phi in nested if (known divergence 3)");
    }
    None
}

/// The `panicked at <file>:<line>` part of a crash, if the process crashed.
fn panic_site(stderr: &str) -> Option<String> {
    let line = stderr.lines().find(|l| l.contains("panicked at "))?;
    let site = &line[line.find("panicked at ")?..];
    let site = site.trim_end_matches(':');
    // Drop the column.
    Some(site.rsplit_once(':').map_or(site, |(a, _)| a).to_owned())
}

fn compare(source: &str) -> Verdict {
    let outcomes: Vec<(Backend, Outcome)> =
        Backend::ALL.iter().map(|&b| (b, run(source, b))).collect();
    let reference = &outcomes[0].1;
    if reference.exit != Some(0) {
        return Verdict::Invalid(format!(
            "the reference backend did not run the program (exit {:?}):\n{}",
            reference.exit, reference.stderr
        ));
    }
    let jit = &outcomes[2].1;
    if jit.stderr.contains("note: running on the LLVM backend")
        && !jit.stderr.contains("Cranelift rejected the function")
    {
        return Verdict::Invalid(format!(
            "`--jit` fell back to LLVM, so the program is outside the Cranelift subset:\n{}",
            jit.stderr
        ));
    }
    let mut report = String::new();
    let mut signature = String::new();
    let mut known = Vec::new();
    for (backend, outcome) in &outcomes[1..] {
        let rejected = *backend == Backend::Cranelift && !ran_on_cranelift(outcome);
        if outcome.observable() != reference.observable() || rejected {
            if let Some(name) = known_divergence(*backend, outcome) {
                known.push(name);
                continue;
            }
            report.push_str(&describe(Backend::ALL[0], reference, *backend, outcome));
            let kind = if let Some(site) = panic_site(&outcome.stderr) {
                format!("crash {site}")
            } else if rejected {
                "rejected by the Cranelift verifier".to_owned()
            } else {
                "different output".to_owned()
            };
            signature.push_str(&format!("[{}: {kind}]", backend.name()));
        }
    }
    if report.is_empty() {
        Verdict::Agree { known }
    } else {
        Verdict::Diverge { signature, report }
    }
}

fn describe(a: Backend, ao: &Outcome, b: Backend, bo: &Outcome) -> String {
    let al: Vec<&str> = ao.stdout.lines().collect();
    let bl: Vec<&str> = bo.stdout.lines().collect();
    let first = (0..al.len().max(bl.len()))
        .find(|&i| al.get(i) != bl.get(i))
        .unwrap_or(0);
    let stderr: Vec<&str> = bo.stderr.lines().take(8).collect();
    format!(
        "{} vs {}: exit {:?} vs {:?}; first differing stdout line {}: {:?} vs {:?}\n\
         stderr of {} (first lines):\n{}\n",
        a.name(),
        b.name(),
        ao.exit,
        bo.exit,
        first + 1,
        al.get(first),
        bl.get(first),
        b.name(),
        stderr.join("\n")
    )
}

/// The leading spaces of a line.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The lines of the statement that starts at line `i`: one line, or, for a
/// line that opens a block, through the `}` that closes it at the same
/// indentation (past any `} else {`).
fn statement_span(lines: &[String], i: usize) -> Option<std::ops::Range<usize>> {
    let line = &lines[i];
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('}') || indent(line) == 0 {
        return None;
    }
    // A loop counter's increment (`i3 += 1i32`) and the test that leaves a
    // `loop` on it (`if i3 >= 7i32 {`) are what bound the loop: deleting
    // either would make the program run until the timeout.
    let counter =
        trimmed.starts_with(['i', 'j']) && trimmed[1..].starts_with(|c: char| c.is_ascii_digit());
    let guard = trimmed
        .strip_prefix("if ")
        .is_some_and(|rest| rest.starts_with(['i', 'j']) && rest.contains(" >= "));
    // The `break` inside such a test is protected for the same reason; a
    // conditional `break` can still go, with the `if` that holds it.
    let exit = trimmed == "break" || trimmed.starts_with("break ");
    if (counter && trimmed.ends_with(" += 1i32")) || guard || exit {
        return None;
    }
    if !line.ends_with('{') {
        return Some(i..i + 1);
    }
    let k = indent(line);
    for (j, next) in lines.iter().enumerate().skip(i + 1) {
        if indent(next) == k && next.trim_start() == "}" {
            return Some(i..j + 1);
        }
        if indent(next) < k {
            return None;
        }
    }
    None
}

/// Reduction: first drop `main`'s calls and empty the bodies of functions
/// `main` no longer calls, then delete statements (nested ones too) from the
/// text one at a time, keeping every deletion after which the backends still
/// diverge the same way. Bounded, so a slow reduction cannot hang the test.
fn reduce(program: &Program, signature: &str) -> String {
    let same = |source: &str| matches!(compare(source), Verdict::Diverge { signature: s, .. } if s == signature);
    let mut budget = 600usize;
    let mut best = program.clone();
    let mut i = 0;
    while i < best.calls.len() && budget > 0 {
        let mut candidate = best.clone();
        candidate.calls.remove(i);
        budget -= 1;
        if same(&candidate.render()) {
            best = candidate;
        } else {
            i += 1;
        }
    }
    for f in (0..best.functions.len()).rev() {
        if budget == 0 || best.calls.iter().any(|c| c.function == f) {
            continue;
        }
        let mut candidate = best.clone();
        candidate.functions[f].stmts.clear();
        budget -= 1;
        if same(&candidate.render()) {
            best = candidate;
        }
    }
    let mut lines: Vec<String> = best.render().lines().map(str::to_owned).collect();
    let mut changed = true;
    while changed && budget > 0 {
        changed = false;
        let mut i = 0;
        while i < lines.len() && budget > 0 {
            let Some(span) = statement_span(&lines, i) else {
                i += 1;
                continue;
            };
            let mut candidate = lines.clone();
            candidate.drain(span);
            budget -= 1;
            if same(&(candidate.join("\n") + "\n")) {
                lines = candidate;
                changed = true;
            } else {
                i += 1;
            }
        }
    }
    lines.join("\n") + "\n"
}

fn report_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("differential");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A failure of one seed.
struct Failure {
    seed: u64,
    signature: String,
    message: String,
    /// The program, for a divergence that may be reduced.
    program: Option<Program>,
}

/// Checks one seed; returns the failure, or `None` when the backends agree.
fn check_seed(seed: u64, cfg: &Config, known: &mut Vec<&'static str>) -> Option<Failure> {
    let program = generate(seed, cfg);
    let source = program.render();
    match compare(&source) {
        Verdict::Agree { known: found } => {
            known.extend(found);
            None
        }
        Verdict::Invalid(why) => {
            let path = report_dir().join(format!("invalid-{seed:#x}.code"));
            std::fs::write(&path, &source).unwrap();
            Some(Failure {
                seed,
                signature: "[generator: invalid program]".to_owned(),
                message: format!(
                    "seed {seed:#x}: generator bug, program at {}:\n{why}",
                    path.display()
                ),
                program: None,
            })
        }
        Verdict::Diverge { signature, report } => {
            let path = report_dir().join(format!("diverge-{seed:#x}.code"));
            std::fs::write(&path, &source).unwrap();
            Some(Failure {
                seed,
                message: format!(
                    "seed {seed:#x}: the backends disagree {signature}; program at {}\n{report}",
                    path.display()
                ),
                signature,
                program: Some(program),
            })
        }
    }
}

/// Reduces `failure`'s program and says where the result is.
fn reduce_failure(failure: &mut Failure) {
    let Some(program) = &failure.program else {
        return;
    };
    let seed = failure.seed;
    let before = program.render().lines().count();
    let reduced = reduce(program, &failure.signature);
    let path = report_dir().join(format!("diverge-{seed:#x}.min.code"));
    failure.message.push_str(&format!(
        "reduced from {before} to {} lines: {}\n",
        reduced.lines().count(),
        path.display()
    ));
    std::fs::write(&path, reduced).unwrap();
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| {
            s.strip_prefix("0x")
                .map_or_else(|| s.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
        })
        .unwrap_or(default)
}

/// The generator configuration: the default, plus the switches named in
/// `CODIRA_DIFF_ENABLE` (comma-separated: `float_to_narrow_int`,
/// `calls_in_print_args`, `print_in_constant_functions`), which turn back on
/// constructs left out because of known divergences -- to check whether one
/// is fixed, or to reproduce a seed reported before its exclusion was added.
fn config() -> Config {
    let mut cfg = Config::default();
    let enable = std::env::var("CODIRA_DIFF_ENABLE").unwrap_or_default();
    for name in enable.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        match name {
            "float_to_narrow_int" => cfg.float_to_narrow_int = true,
            "calls_in_print_args" => cfg.calls_in_print_args = true,
            "print_in_constant_functions" => cfg.print_in_constant_functions = true,
            other => panic!("CODIRA_DIFF_ENABLE: unknown switch `{other}`"),
        }
    }
    cfg
}

/// Runs `count` seeds from `base` on `threads` threads, and fails with every
/// failure found, grouped by signature. With `minimise`, the first program of
/// each signature is reduced (one per signature, to keep the time bounded).
fn run_seeds(base: u64, count: u64, threads: usize, minimise: bool) {
    let cfg = config();
    let next = AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::new());
    let known = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed) as u64;
                if i >= count {
                    break;
                }
                let seed = base.wrapping_add(i);
                let mut found = Vec::new();
                if let Some(failure) = check_seed(seed, &cfg, &mut found) {
                    failures.lock().unwrap().push(failure);
                }
                known.lock().unwrap().extend(found);
            });
        }
    });
    let mut failures = failures.into_inner().unwrap();
    failures.sort_by(|a, b| (&a.signature, a.seed).cmp(&(&b.signature, b.seed)));
    if minimise {
        let mut seen = std::collections::HashSet::new();
        for failure in &mut failures {
            if seen.insert(failure.signature.clone()) {
                reduce_failure(failure);
            }
        }
    }
    let mut summary = String::new();
    for failure in &failures {
        summary.push_str(&format!("  {:#x} {}\n", failure.seed, failure.signature));
    }
    let details: Vec<&str> = failures.iter().map(|f| f.message.as_str()).collect();
    assert!(
        failures.is_empty(),
        "{} of {count} programs failed (base seed {base:#x}):\n{summary}\n{}",
        failures.len(),
        details.join("\n")
    );
    let known = known.into_inner().unwrap();
    let mut tally: Vec<(&str, usize)> = Vec::new();
    for name in known {
        match tally.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => tally.push((name, 1)),
        }
    }
    println!("{count} programs agree on all three backends (base seed {base:#x})");
    for (name, c) in tally {
        println!("  except a known divergence in {c} of them: {name}");
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get().min(8))
}

/// The default run: 150 programs from a fixed seed (about 11 s on four cores;
/// every divergence found is reduced, one per kind).
#[test]
fn backends_agree_on_random_programs() {
    let base = env_u64("CODIRA_DIFF_SEED", DEFAULT_SEED);
    let count = env_u64("CODIRA_DIFF_PROGRAMS", 150);
    run_seeds(base, count, threads(), true);
}

/// A long run: `CODIRA_DIFF_PROGRAMS` programs (2,000 by default), e.g.
/// `CODIRA_DIFF_PROGRAMS=10000 CODIRA_DIFF_SEED=0x1234 cargo test -p codira
/// --test differential -- --ignored`. Not reduced, to keep it fast; rerun a
/// reported seed with `CODIRA_DIFF_SEED=<seed> CODIRA_DIFF_PROGRAMS=1` to
/// reduce it.
#[test]
#[ignore = "long: thousands of programs"]
fn many_programs() {
    let base = env_u64("CODIRA_DIFF_SEED", DEFAULT_SEED ^ 0xFFFF_0000);
    let count = env_u64("CODIRA_DIFF_PROGRAMS", 2_000);
    run_seeds(base, count, threads(), false);
}

/// The generator is a function of its seed, so a reported seed reproduces.
#[test]
fn generator_is_deterministic() {
    let cfg = Config::default();
    for seed in 0..20 {
        assert_eq!(generate(seed, &cfg).render(), generate(seed, &cfg).render());
    }
}

/// The default seeds exercise every construct the generator knows, so a
/// change to it that silently stops generating one is caught.
#[test]
fn generator_covers_the_subset() {
    let cfg = Config::default();
    let base = env_u64("CODIRA_DIFF_SEED", DEFAULT_SEED);
    let count = env_u64("CODIRA_DIFF_PROGRAMS", 150);
    let all: String = (0..count)
        .map(|i| generate(base.wrapping_add(i), &cfg).render())
        .collect();
    for construct in [
        "while ",
        "loop {",
        "break\n",
        "break (",
        "return ",
        " << (",
        " >> (",
        " / (",
        " % (",
        " as i8)",
        " as u64)",
        " as f32)",
        " as f64)",
        "(if ",
        "(-",
        "(~",
        " && ",
        " || ",
        "(!",
        " += ",
        " ^= ",
        "<<= (",
        ": i16 = ",
        ": u32 = ",
        ": bool = ",
        " == ",
        " >= ",
    ] {
        assert!(
            all.contains(construct),
            "no program of the default seeds contains `{construct}`"
        );
    }
    // Calls between generated functions, not only from `main`.
    assert!(
        all.lines()
            .filter(|l| !l.trim_start().starts_with("let r"))
            .any(|l| l.contains("f0(") || l.contains("f1(") || l.contains("f2(")),
        "no generated function calls another"
    );
}

/// KNOWN DIVERGENCE: the backends print floats differently.
///
/// The LLVM backend's runtime prints a float with Rust's `{:?}` (shortest
/// round trip, always with a decimal point or exponent, and an `f32` in
/// `f32` precision: `crates/codira_runtime/src/builtins.rs`, `print_f32` and
/// `print_f64`). The Cranelift backend's host print function uses `{}` and
/// widens an `f32` to `f64` first (`crates/codira_cranelift/src/lib.rs`,
/// `codira_jit_print_f64` and `Translator::print`). So `println(1.0)` prints
/// `1.0` on LLVM and `1` on Cranelift, and `println(0.1f32)` prints `0.1` and
/// `0.10000000149011612`.
///
/// The generator therefore never prints a float directly (it prints integer
/// conversions of each float result, which still compares the arithmetic).
/// This test pins the divergence down: once the Cranelift backend is fixed it
/// fails, and then it should become an agreement test and the generator
/// should print floats directly.
#[test]
fn known_divergence_float_printing() {
    let source =
        "public func main() {\n    println(1.0)\n    println(0.1f32)\n    println(2.5)\n}\n";
    let llvm = run(source, Backend::LlvmDefault);
    let jit = run(source, Backend::Cranelift);
    assert_eq!(llvm.exit, Some(0), "{}", llvm.stderr);
    assert!(ran_on_cranelift(&jit), "{}", jit.stderr);
    assert_eq!(llvm.stdout, "1.0\n0.1\n2.5\n");
    assert_eq!(
        jit.stdout, "1\n0.10000000149011612\n2.5\n",
        "the float printing divergence changed or was fixed: update this test, \
         the generator"
    );
}

/// KNOWN DIVERGENCE: a cast from a float to an 8- or 16-bit integer crashes
/// the Cranelift backend.
///
/// `Translator::cast` in `crates/codira_cranelift/src/lib.rs` emits
/// `fcvt_to_sint_sat` / `fcvt_to_uint_sat` with an `i8` or `i16` result, which
/// Cranelift's x86-64 backend does not encode: it panics with "internal error:
/// entered unreachable code" in `cranelift-codegen` 0.136.2,
/// `src/isa/x64/inst/emit.rs` (line 1057 for a signed target, 1247 for an
/// unsigned one). The panic is not caught, so `codira run --jit` exits with
/// status 101 instead of compiling the program or falling back to LLVM. The
/// LLVM backend runs it. 32- and 64-bit targets work on both.
///
/// The generator does not write these casts by default
/// (`Config::float_to_narrow_int`). Once the backend is fixed this test fails;
/// then turn the option on and make this an agreement test.
#[test]
fn known_divergence_float_to_narrow_int_cast() {
    for (from, to) in [("f64", "i8"), ("f32", "u8"), ("f64", "i16"), ("f32", "u16")] {
        let source = format!(
            "func c(x: {from}) -> {to} {{\n    (x as {to})\n}}\n\
             public func main() {{\n    println(c(300.5{from}))\n}}\n"
        );
        let llvm = run(&source, Backend::LlvmDefault);
        assert_eq!(llvm.exit, Some(0), "{}", llvm.stderr);
        let jit = run(&source, Backend::Cranelift);
        assert!(
            jit.exit == Some(101) && jit.stderr.contains("entered unreachable code"),
            "the {from} -> {to} cast no longer crashes the Cranelift backend \
             (exit {:?}): update this test, the generator\n{}",
            jit.exit,
            jit.stderr
        );
    }
}

/// KNOWN DIVERGENCE: `-O 0` builds of a nested `if` crash the LLVM backend.
///
/// An `if`/`else` that is the only statement of an `if` without `else`, in
/// the `else` branch of another `if`, makes the LLVM backend emit a unit-typed
/// `phi {}` whose operand does not dominate its use. At `-O 0` the module is
/// verified when inkwell clones it, and `codira run -O 0` panics with
/// "Instruction does not dominate all uses!". At `-O 1` to `-O 3` the same
/// program builds and runs correctly; whether the IR before optimization is
/// invalid there too was not established (the IR written by `--emit-ir` is
/// after optimization). The differential test recognises exactly this error
/// on `-O 0` as a known divergence (`known_divergence`) and still compares
/// the other two backends on such programs.
#[test]
fn known_divergence_llvm_o0_nested_if() {
    let source = "func f(a: bool, b: bool, c: bool) {\n    if a {\n        println(\"a\")\n    } else {\n        if b {\n            if c {\n                println(\"c\")\n            } else {\n                println(\"not c\")\n            }\n        }\n    }\n}\n\
                  public func main() {\n    f(false, true, true)\n}\n";
    let o2 = run(source, Backend::LlvmDefault);
    assert_eq!(
        (o2.exit, o2.stdout.as_str()),
        (Some(0), "c\n"),
        "{}",
        o2.stderr
    );
    let jit = run(source, Backend::Cranelift);
    assert_eq!(jit.stdout, "c\n", "{}", jit.stderr);
    let o0 = run(source, Backend::LlvmO0);
    assert!(
        known_divergence(Backend::LlvmO0, &o0).is_some(),
        "the -O 0 nested-if crash changed or was fixed (exit {:?}): update this \
         test, `known_divergence`\n{}",
        o0.exit,
        o0.stderr
    );
}

/// KNOWN DIVERGENCE: `println` arguments with side effects.
///
/// The LLVM backend evaluates every argument of `println` before printing any
/// of them; the Cranelift backend (`Translator::print`) evaluates and prints
/// them one at a time. So when an argument calls a function that prints,
/// the output is interleaved differently. `agents/USING_CODIRA.md` does not
/// say which order is meant. The generator does not put calls in `println`
/// arguments by default (`Config::calls_in_print_args`).
#[test]
fn known_divergence_print_argument_order() {
    let source = "func g() -> i32 {\n    println(\"in g\")\n    7i32\n}\n\
                  public func main() {\n    println(\"g is \", g())\n}\n";
    let llvm = run(source, Backend::LlvmDefault);
    let jit = run(source, Backend::Cranelift);
    assert!(ran_on_cranelift(&jit), "{}", jit.stderr);
    assert_eq!(llvm.stdout, "in g\ng is 7\n", "{}", llvm.stderr);
    assert_eq!(
        jit.stdout, "g is in g\n7\n",
        "the print argument order divergence changed or was fixed: update this \
         test, the generator"
    );
}

/// KNOWN DIVERGENCE (wrong code on the LLVM path): the Eidos constant fold
/// drops output.
///
/// In a release-mode build (the default for `codira run` and `codira
/// build`), `crates/codira_codegen/src/eidos_fold.rs` replaces a function
/// without parameters whose result evaluates to a constant by `ret
/// <constant>`. It does not check that the body is free of side effects, so
/// the `println` in `g` -- reached through `f`, whose result does not depend
/// on `g`'s -- is never executed. `--mode dev` (no Eidos) and the Cranelift
/// backend print it. The fold's documentation says a declined fold is "never
/// a wrong answer"; this is a wrong answer. Found by `many_programs` (seed
/// base `0x5eed0000`, 7 of 10,000 programs). The generator keeps functions
/// without parameters from printing by default
/// (`Config::print_in_constant_functions`).
#[test]
fn known_divergence_eidos_fold_drops_output() {
    let source = "func g() -> i8 {\n    println(\"side effect\")\n    1i8\n}\n\
                  func f() -> i64 {\n    let unused: i8 = g()\n    7i64\n}\n\
                  public func main() {\n    println(f())\n}\n";
    let jit = run(source, Backend::Cranelift);
    assert!(ran_on_cranelift(&jit), "{}", jit.stderr);
    assert_eq!(jit.stdout, "side effect\n7\n");
    for backend in [Backend::LlvmDefault, Backend::LlvmO0] {
        let llvm = run(source, backend);
        assert_eq!(
            llvm.stdout,
            "7\n",
            "{}: the Eidos fold divergence changed or was fixed: update this test, \
             the generator and RFC 0008\n{}",
            backend.name(),
            llvm.stderr
        );
    }
}
