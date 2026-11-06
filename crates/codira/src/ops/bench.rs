//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira bench`: build the package with its `benches/` directory,
//!   optimised, and time every `@bench` function, each in its own process, with
//!   warm-up, robust statistics and comparison against a saved baseline.
//!
//! Discovery follows `codira test`: `src/` plus every `.code` file under
//! `benches/` (mounted under a `benches` module) are compiled together, and
//! every `public` function marked `@bench` there is a benchmark. A benchmark
//! returns `()`, `bool`, an integer or a float, and takes no parameters (it
//! is called once per iteration) or one `i64`, the number of iterations it
//! runs itself (so the cost of a call through the runtime is paid once per
//! sample, not per iteration). Its result is received by the caller, so the
//! work that produces it is not optimised away.
//!
//! For each benchmark a child `codira bench --run-one` process loads the
//! library and
//!
//! 1. warms up: calls it repeatedly for the warm-up time (300 ms by default),
//!    which also estimates the time per call;
//! 2. picks a number of calls per sample so that the samples together take the
//!    measurement time (1 s by default);
//! 3. takes the samples (50 by default), timing each batch of calls with the
//!    monotonic clock.
//!
//! The parent reports, per call: the median, the median absolute deviation,
//! a 95% confidence interval for the median, the mean without severe
//! outliers, and how many samples lie outside Tukey's fences. Every time
//! includes the cost of a call through the runtime; an empty benchmark is
//! measured the same way and reported as the call overhead.
//!
//! Each benchmark is measured in `--rounds` separate processes (3 by
//! default), each taking its share of the samples. Samples within one
//! process are not independent of each other: CPU frequency, code and data
//! layout and the machine's load shift a whole process. The spread of the
//! round medians measures that shift.
//!
//! `--save-baseline <name>` stores the samples under
//! `target/bench/baselines/<name>.json`; `--baseline <name>` compares with
//! them, reporting a benchmark as regressed or improved only when the
//! Mann-Whitney U test rejects "no change" at p < 0.05 *and* the median
//! moved by more than both `--threshold` percent (2 by default) and the
//! spread between rounds of either run. A regression makes the command
//! fail.

use std::{
    io::stderr,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use codira_compiler::{Config, Driver, RelativePathBuf, Target};
use codira_hir::{HasSource, HirDatabase, Package};
use codira_hir_input::{SourceDatabase, SourceRootId};
use codira_runtime::Runtime;
use codira_syntax::{AstNode, SyntaxKind};

use super::bench_stats::{self as stats, Verdict};
use crate::{
    ops::{
        build::{display_colors, locate_manifest, UseColor},
        fmt::collect_code_files,
        heap::{HeapArgs, HeapSettings},
    },
    ExitStatus,
};

const RESULT_PREFIX: &str = "codira-bench-result:";
const ERROR_PREFIX: &str = "codira-bench-error:";
/// The empty benchmark that measures the cost of a call through the
/// runtime.
const OVERHEAD_FILE: &str = "benches/codira_bench_call_overhead.code";
const OVERHEAD_NAME: &str = "codira_bench_call_overhead";
const BASELINE_SCHEMA: &str = "codira-bench-baseline/1";

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the project
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Run only the benchmarks whose name contains this text
    filter: Option<String>,

    /// Warm-up time per benchmark, in milliseconds
    #[clap(long, default_value_t = 300)]
    warm_up_ms: u64,

    /// Measurement time per benchmark, in milliseconds
    #[clap(long, default_value_t = 1000)]
    measurement_ms: u64,

    /// Number of samples per benchmark
    #[clap(long, default_value_t = 50)]
    samples: usize,

    /// Number of separate processes each benchmark is measured in; their
    /// spread is the noise a change must exceed to count
    #[clap(long, default_value_t = 3)]
    rounds: usize,

    /// Save the results as the baseline with this name
    #[clap(long, value_name = "NAME")]
    save_baseline: Option<String>,

    /// Compare with the baseline with this name; fail on a regression
    #[clap(long, value_name = "NAME")]
    baseline: Option<String>,

    /// The smallest change of the median, in percent, reported as a
    /// regression or improvement
    #[clap(long, default_value_t = 2.0)]
    threshold: f64,

    /// Optimisation level (0-3)
    #[clap(long, short = 'O', default_value_t = 3)]
    opt_level: u8,

    /// Use color in diagnostics
    #[clap(long, value_enum)]
    color: Option<UseColor>,

    /// Internal: run the single benchmark `--entry` from this library and
    /// report its samples on stdout
    #[clap(long, hide = true, requires = "entry")]
    run_one: Option<PathBuf>,

    /// Internal: the benchmark function `--run-one` calls
    #[clap(long, hide = true)]
    entry: Option<String>,

    #[clap(flatten)]
    heap: HeapArgs,
}

/// One benchmark's samples: nanoseconds per call.
#[derive(Debug, Clone)]
struct Measured {
    name: String,
    per_call_ns: Vec<f64>,
    calls_per_sample: u64,
    /// The median of each round (process) separately.
    round_medians: Vec<f64>,
}

impl Measured {
    /// The spread of the round medians relative to the overall median: how
    /// far the result moves between processes with nothing changed.
    fn noise(&self) -> f64 {
        stats::relative_spread(&self.round_medians)
    }
}

pub fn bench(args: Args) -> anyhow::Result<ExitStatus> {
    if let (Some(library), Some(entry)) = (&args.run_one, &args.entry) {
        return run_one(library, entry, &args);
    }
    if args.rounds == 0 || args.samples < 2 * args.rounds {
        return Err(anyhow!(
            "--rounds must be at least 1 and --samples at least twice --rounds"
        ));
    }

    let manifest_path = locate_manifest(args.manifest_path.as_deref())?;
    let package = codira_project::Package::from_file(&manifest_path)?;
    // Each benchmark resolves the settings again in its process; reading them
    // here reports a bad value once, before the build, and warns once.
    let heap = HeapSettings::resolve(
        &args.heap,
        Some((package.manifest().runtime(), &manifest_path)),
    )?;
    heap.warn_if_unlimited();
    let root = package.root().to_path_buf();
    let baseline_dir = root.join("target").join("bench").join("baselines");
    // Read the baseline first: a missing one is an error before the build.
    let baseline = match &args.baseline {
        Some(name) => Some(read_baseline(&baseline_dir, name)?),
        None => None,
    };

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: super::build::optimization_level(args.opt_level)?,
        out_dir: Some(root.join("target").join("bench")),
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    };
    let (_, mut driver) = Driver::with_package_path(&manifest_path, config)?;
    for (virtual_path, contents) in bench_sources(&root.join("benches"))? {
        driver.add_file(virtual_path, contents);
    }
    driver.add_file(
        RelativePathBuf::from(OVERHEAD_FILE),
        format!("@bench\npublic func {OVERHEAD_NAME}() {{}}\n"),
    );

    let errors = error_count(&driver);
    if errors > 0 {
        driver.emit_diagnostics(&mut stderr(), display_colors(args.color))?;
        return Ok(ExitStatus::Error);
    }
    driver.write_all_assemblies(true)?;

    let db = driver.db();
    // (name shown and saved, name the runtime knows it by, library)
    let mut benches: Vec<(String, String, PathBuf)> = Vec::new();
    let mut private = Vec::new();
    let mut overhead: Option<(String, PathBuf)> = None;
    for package in Package::all(db) {
        for module in package.modules(db) {
            for function in module.all_functions(db) {
                if db.file_source_root(function.file_id(db)) != SourceRootId(0)
                    || !is_bench(db, function)
                {
                    continue;
                }
                let symbol = function.full_name(db);
                let name = display_name(db, function);
                let excluded = args
                    .filter
                    .as_ref()
                    .is_some_and(|f| !name.contains(f.as_str()));
                if excluded && function.name(db).to_string() != OVERHEAD_NAME {
                    continue;
                }
                if !is_public(db, function) {
                    private.push(name);
                    continue;
                }
                let library = driver.assembly_output_path(module);
                if function.name(db).to_string() == OVERHEAD_NAME {
                    overhead = Some((symbol, library));
                    continue;
                }
                benches.push((name, symbol, library));
            }
        }
    }
    benches.sort();
    for name in &private {
        println!("bench {name} ... FAILED: a benchmark must be `public` (like a test)");
    }
    if benches.is_empty() && private.is_empty() {
        println!("no @bench functions found in src/ or benches/");
        return Ok(ExitStatus::Success);
    }

    let exe = std::env::current_exe()?;
    let round_ms = (args.measurement_ms / args.rounds as u64).max(1);
    let round_samples = args.samples / args.rounds;
    let round =
        |name: &str, symbol: &str, library: &Path| -> anyhow::Result<Result<Measured, String>> {
            let output = Command::new(&exe)
                .arg("bench")
                .arg("--run-one")
                .arg(library)
                .arg("--entry")
                .arg(symbol)
                .arg("--warm-up-ms")
                .arg(args.warm_up_ms.to_string())
                .arg("--measurement-ms")
                .arg(round_ms.to_string())
                .arg("--samples")
                .arg(round_samples.to_string())
                .arg("--rounds")
                .arg("1")
                .arg("--manifest-path")
                .arg(&manifest_path)
                .args(args.heap.forward())
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(error) = stdout.lines().find_map(|l| l.strip_prefix(ERROR_PREFIX)) {
                return Ok(Err(error.trim().to_string()));
            }
            let Some(line) = stdout.lines().find_map(|l| l.strip_prefix(RESULT_PREFIX)) else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let last = stderr
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                return Ok(Err(format!("crashed ({}): {last}", output.status)));
            };
            let mut fields = line.split_whitespace().map(str::parse::<f64>);
            let calls = fields.next().and_then(Result::ok).unwrap_or(1.0).max(1.0);
            let per_call_ns: Vec<f64> =
                fields.filter_map(Result::ok).map(|ns| ns / calls).collect();
            Ok(Ok(Measured {
                name: name.to_string(),
                round_medians: vec![stats::median(&per_call_ns)],
                per_call_ns,
                calls_per_sample: calls as u64,
            }))
        };
    // Every round in its own process; the samples are pooled.
    let child =
        |name: &str, symbol: &str, library: &Path| -> anyhow::Result<Result<Measured, String>> {
            let mut pooled: Option<Measured> = None;
            for _ in 0..args.rounds {
                let m = match round(name, symbol, library)? {
                    Ok(m) => m,
                    Err(e) => return Ok(Err(e)),
                };
                match &mut pooled {
                    None => pooled = Some(m),
                    Some(p) => {
                        p.per_call_ns.extend(m.per_call_ns);
                        p.round_medians.extend(m.round_medians);
                        p.calls_per_sample = p.calls_per_sample.max(m.calls_per_sample);
                    }
                }
            }
            Ok(Ok(pooled.expect("at least one round")))
        };

    if let Some((symbol, library)) = &overhead {
        if let Ok(m) = child(OVERHEAD_NAME, symbol, library)? {
            println!(
                "call overhead: {} per call through the runtime (included in every iteration of a \
                 benchmark without an iteration parameter, once per sample otherwise)",
                format_ns(stats::median(&m.per_call_ns))
            );
        }
    }
    println!(
        "running {} benchmark(s): {} round(s) of {} ms warm-up and {} ms of {} samples each, -O{}\n",
        benches.len(),
        args.rounds,
        args.warm_up_ms,
        round_ms,
        round_samples,
        args.opt_level
    );

    let mut results = Vec::new();
    let mut failed = private.len();
    let mut regressed = 0;
    for (name, symbol, library) in &benches {
        match child(name, symbol, library)? {
            Err(why) => {
                println!("bench {name} ... FAILED: {why}");
                failed += 1;
            }
            Ok(m) => {
                print_result(&m);
                if let Some(base) = &baseline {
                    match base.iter().find(|b| b.name == m.name) {
                        Some(old) => {
                            // A change must exceed the threshold and the
                            // noise between rounds of either run.
                            let noise = old.noise().max(m.noise());
                            let threshold = (args.threshold / 100.0).max(noise);
                            let (verdict, change, p) =
                                stats::compare(&old.per_call_ns, &m.per_call_ns, 0.05, threshold);
                            let word = match verdict {
                                Verdict::Regressed => {
                                    regressed += 1;
                                    "REGRESSED"
                                }
                                Verdict::Improved => "improved",
                                Verdict::NoChange => "no change",
                            };
                            println!(
                                "    vs baseline: {:+.1}% median ({} -> {}), p = {p:.3}, noise \
                                 ±{:.1}%: {word}",
                                change * 100.0,
                                format_ns(stats::median(&old.per_call_ns)),
                                format_ns(stats::median(&m.per_call_ns)),
                                threshold * 100.0
                            );
                        }
                        None => println!("    vs baseline: not in the baseline"),
                    }
                }
                results.push(m);
            }
        }
    }

    if let Some(name) = &args.save_baseline {
        if results.is_empty() {
            println!("\nnothing was measured; baseline '{name}' was not saved");
        } else {
            let path = write_baseline(&baseline_dir, name, &results)?;
            println!("\nsaved baseline '{name}' to {}", path.display());
        }
    }
    println!(
        "\nbench result: {} measured, {failed} failed{}",
        results.len(),
        if baseline.is_some() {
            format!(", {regressed} regressed")
        } else {
            String::new()
        }
    );
    Ok(ExitStatus::from(failed == 0 && regressed == 0))
}

fn print_result(m: &Measured) {
    let xs = &m.per_call_ns;
    let (lo, hi) = stats::median_ci(xs);
    let (mild, severe) = stats::outliers(xs);
    println!(
        "bench {} ... median {} ± {} (MAD), 95% CI [{}, {}], mean {}",
        m.name,
        format_ns(stats::median(xs)),
        format_ns(stats::mad(xs)),
        format_ns(lo),
        format_ns(hi),
        format_ns(stats::trimmed_mean(xs)),
    );
    println!(
        "    {} samples of {} call(s) in {} round(s), spread between rounds {:.1}%; outliers: \
         {mild} mild, {severe} severe",
        xs.len(),
        m.calls_per_sample,
        m.round_medians.len(),
        m.noise() * 100.0
    );
}

pub(crate) fn format_ns(ns: f64) -> String {
    if ns >= 1e9 {
        format!("{:.3} s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.3} ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.3} µs", ns / 1e3)
    } else {
        format!("{ns:.2} ns")
    }
}

/// `benches.work.looped`: the module path from the package root, then the
/// function's name.
fn display_name(db: &dyn HirDatabase, function: codira_hir::Function) -> String {
    let mut parts: Vec<String> = function
        .module(db)
        .path_to_root(db)
        .iter()
        .filter_map(|m| m.name(db))
        .collect();
    parts.reverse();
    parts.push(function.name(db).to_string());
    parts.join(".")
}

/// Whether the function is declared `public`.
fn is_public(db: &dyn HirDatabase, function: codira_hir::Function) -> bool {
    function
        .source(db)
        .value
        .syntax()
        .children()
        .any(|c| c.kind() == SyntaxKind::VISIBILITY && c.text().to_string().trim() == "public")
}

/// Whether the function is marked `@bench`.
fn is_bench(db: &dyn HirDatabase, function: codira_hir::Function) -> bool {
    let src = function.source(db);
    src.value
        .syntax()
        .children()
        .filter(|c| c.kind() == SyntaxKind::ATTRIBUTE_LIST)
        .flat_map(|l| l.children().collect::<Vec<_>>())
        .any(|attribute| {
            attribute
                .descendants_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| t.kind() == SyntaxKind::IDENT)
                .is_some_and(|t| t.text() == "bench")
        })
}

/// Errors that stop the run: syntax errors and every diagnostic.
fn error_count(driver: &Driver) -> usize {
    let db = driver.db();
    let mut count = 0;
    for package in Package::all(db) {
        for module in package.modules(db) {
            if let Some(file_id) = module.file_id(db) {
                count += codira_hir::AstDatabase::parse(db, file_id).errors().len();
            }
            module.diagnostics(
                db,
                &mut codira_hir::DiagnosticSink::new(|_| {
                    count += 1;
                }),
            );
        }
    }
    count
}

/// Every `.code` file under `benches_dir`, mounted at `benches/<path>`.
fn bench_sources(benches_dir: &Path) -> anyhow::Result<Vec<(RelativePathBuf, String)>> {
    let mut out = Vec::new();
    if !benches_dir.is_dir() {
        return Ok(out);
    }
    let mut files = Vec::new();
    collect_code_files(benches_dir, &mut files)?;
    for path in files {
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("could not read '{}': {e}", path.display()))?;
        let relative = path.strip_prefix(benches_dir).unwrap_or(&path);
        out.push((
            RelativePathBuf::from(format!(
                "benches/{}",
                relative.display().to_string().replace('\\', "/")
            )),
            contents,
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// A function that calls the benchmark `n` times and returns the elapsed
/// time.
type Timer<'a> = Box<dyn Fn(u64) -> Result<Duration, String> + 'a>;

/// The child side: warm up, size the samples, take them, print them.
fn run_one(library: &Path, entry: &str, args: &Args) -> anyhow::Result<ExitStatus> {
    codira_healing::trap::install();
    let heap = match &args.manifest_path {
        Some(path) => {
            let package = codira_project::Package::from_file(path)?;
            HeapSettings::resolve(&args.heap, Some((package.manifest().runtime(), path)))?
        }
        None => HeapSettings::resolve(&args.heap, None)?,
    };
    // Safety: the library was just compiled by the parent `codira bench`.
    let runtime = unsafe { heap.apply(Runtime::builder(library)).finish() }?;
    let definition = runtime
        .get_function_definition(entry)
        .ok_or_else(|| anyhow!("benchmark `{entry}` is not in '{}'", library.display()))?;
    let ret = &definition.prototype.signature.return_type;
    let params = &definition.prototype.signature.arg_types;
    // `func b()` is called once per iteration; `func b(n: i64)` runs its
    // own loop of `n` iterations, so the cost of a call through the
    // runtime is paid once per sample instead of once per iteration.
    let iterating = match params.as_slice() {
        [] => false,
        [n] if n.equals::<i64>() => true,
        _ => {
            println!(
                "{ERROR_PREFIX} a benchmark takes no parameters, or one `i64`: the number of \
                 iterations to run"
            );
            return Ok(ExitStatus::Error);
        }
    };

    macro_rules! timer {
        ($($ty:ty),*) => {{
            let mut timer: Option<Timer<'_>> = None;
            $(
                if timer.is_none() && ret.equals::<$ty>() {
                    let runtime = &runtime;
                    timer = Some(if iterating {
                        Box::new(move |n: u64| {
                            let n = i64::try_from(n).map_err(|e| e.to_string())?;
                            let start = Instant::now();
                            let value = runtime
                                .invoke::<$ty, (i64,)>(entry, (n,))
                                .map_err(|e| e.to_string())?;
                            let elapsed = start.elapsed();
                            std::hint::black_box(value);
                            Ok(elapsed)
                        })
                    } else {
                        Box::new(move |n: u64| {
                            let start = Instant::now();
                            for _ in 0..n {
                                let value = runtime
                                    .invoke::<$ty, ()>(entry, ())
                                    .map_err(|e| e.to_string())?;
                                std::hint::black_box(value);
                            }
                            Ok(start.elapsed())
                        })
                    });
                }
            )*
            timer
        }};
    }
    let Some(timer) = timer!((), bool, i8, i16, i32, i64, u8, u16, u32, u64, f32, f64) else {
        println!(
            "{ERROR_PREFIX} a benchmark returns (), bool, an integer or a float, not {}",
            ret.name()
        );
        return Ok(ExitStatus::Error);
    };

    let outcome = codira_healing::trap::protected(|| -> Result<(u64, Vec<u128>), String> {
        // Warm up, doubling the batch, until the warm-up time is spent.
        let warm_up = Duration::from_millis(args.warm_up_ms);
        let mut calls = 0u64;
        let mut spent = Duration::ZERO;
        let mut batch = 1u64;
        while spent < warm_up || calls == 0 {
            spent += timer(batch)?;
            calls += batch;
            batch = batch.saturating_mul(2).min(1 << 20);
        }
        let per_call_ns = (spent.as_nanos() as f64 / calls as f64).max(1.0);
        let sample_ns = args.measurement_ms as f64 * 1e6 / args.samples as f64;
        let calls_per_sample = ((sample_ns / per_call_ns).round() as u64).max(1);
        let mut samples = Vec::with_capacity(args.samples);
        for _ in 0..args.samples {
            samples.push(timer(calls_per_sample)?.as_nanos());
        }
        Ok((calls_per_sample, samples))
    });
    match outcome {
        Ok(Ok((calls, samples))) => {
            let samples: Vec<String> = samples.iter().map(u128::to_string).collect();
            println!("{RESULT_PREFIX} {calls} {}", samples.join(" "));
            Ok(ExitStatus::Success)
        }
        Ok(Err(e)) => {
            println!("{ERROR_PREFIX} could not be called: {e}");
            Ok(ExitStatus::Error)
        }
        Err(fault) => {
            println!("{ERROR_PREFIX} trapped ({:?})", fault.kind);
            Ok(ExitStatus::Error)
        }
    }
}

fn baseline_path(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        return Err(anyhow!(
            "a baseline name is letters, digits, '-', '_' and '.', not '{name}'"
        ));
    }
    Ok(dir.join(format!("{name}.json")))
}

fn write_baseline(dir: &Path, name: &str, results: &[Measured]) -> anyhow::Result<PathBuf> {
    let path = baseline_path(dir, name)?;
    std::fs::create_dir_all(dir)?;
    let benches: Vec<serde_json::Value> = results
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "calls_per_sample": m.calls_per_sample,
                "median_ns": stats::median(&m.per_call_ns),
                "mad_ns": stats::mad(&m.per_call_ns),
                "round_medians_ns": m.round_medians,
                "per_call_ns": m.per_call_ns,
            })
        })
        .collect();
    let json = serde_json::json!({ "schema": BASELINE_SCHEMA, "benches": benches });
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(path)
}

fn read_baseline(dir: &Path, name: &str) -> anyhow::Result<Vec<Measured>> {
    let path = baseline_path(dir, name)?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        anyhow!(
            "no baseline '{name}' ({}: {e}); save one with --save-baseline {name}",
            path.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow!("baseline '{}' is not valid JSON: {e}", path.display()))?;
    if json["schema"] != BASELINE_SCHEMA {
        return Err(anyhow!(
            "baseline '{}' is not a {BASELINE_SCHEMA} file",
            path.display()
        ));
    }
    let benches = json["benches"]
        .as_array()
        .ok_or_else(|| anyhow!("baseline '{}' has no benches", path.display()))?;
    benches
        .iter()
        .map(|b| {
            let name = b["name"]
                .as_str()
                .ok_or_else(|| anyhow!("a bench in '{}' has no name", path.display()))?;
            let per_call_ns: Vec<f64> = b["per_call_ns"]
                .as_array()
                .map(|xs| xs.iter().filter_map(serde_json::Value::as_f64).collect())
                .unwrap_or_default();
            if per_call_ns.len() < 2 {
                return Err(anyhow!(
                    "bench '{name}' in '{}' has no samples",
                    path.display()
                ));
            }
            let round_medians: Vec<f64> = b["round_medians_ns"]
                .as_array()
                .map(|xs| xs.iter().filter_map(serde_json::Value::as_f64).collect())
                .unwrap_or_default();
            Ok(Measured {
                name: name.to_string(),
                per_call_ns,
                calls_per_sample: b["calls_per_sample"].as_u64().unwrap_or(1),
                round_medians,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_round_trip_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let m = Measured {
            name: "benches.math.bench_add".to_string(),
            per_call_ns: vec![10.0, 11.0, 12.0],
            calls_per_sample: 1000,
            round_medians: vec![10.0, 12.0],
        };
        write_baseline(dir.path(), "main", &[m]).unwrap();
        let back = read_baseline(dir.path(), "main").unwrap();
        assert_eq!(back[0].name, "benches.math.bench_add");
        assert_eq!(back[0].per_call_ns, [10.0, 11.0, 12.0]);
        assert_eq!(back[0].round_medians, [10.0, 12.0]);
        assert!((back[0].noise() - 2.0 / 11.0).abs() < 1e-9);
        assert!(read_baseline(dir.path(), "missing").is_err());
        assert!(baseline_path(dir.path(), "../x").is_err());
        assert!(baseline_path(dir.path(), "").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(format_ns(12.345), "12.35 ns");
        assert_eq!(format_ns(1_500.0), "1.500 µs");
        assert_eq!(format_ns(2_000_000.0), "2.000 ms");
        assert_eq!(format_ns(3e9), "3.000 s");
    }
}
