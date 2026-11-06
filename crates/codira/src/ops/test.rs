//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - `codira test`: compile the package together with its `tests/` directory
//!   and run every `@test` function, each in its own process.
//!
//! What it does, in order:
//!
//! 1. refreshes `tests/generated/` from the verifier's current counterexamples
//!    (see `ops::counterexample`);
//! 2. compiles `src/` plus every `.code` file under `tests/`. A generated
//!    counterexample test `math.div_counterexample.code` is mounted as a child
//!    module of `math`, so it can call a private `div` through `super`; any
//!    other test file is mounted under a `tests` module of the package root;
//! 3. reports diagnostics. Unproven contracts do not stop the run -- the
//!    generated tests exist precisely to run against code whose contract does
//!    not hold -- but every other error does;
//! 4. runs each `@test` in a child `codira test --run-one` process, so a test
//!    that traps or aborts takes down only itself. A test passes when it
//!    returns `true` (or, returning `()`, simply returns).

use std::{
    io::stderr,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::anyhow;
use codira_compiler::{Config, Driver, RelativePathBuf, Target};
use codira_hir::{diagnostics::ContractViolation, DiagnosticSink, Package};
use codira_runtime::Runtime;

use crate::{
    ops::{
        build::{display_colors, locate_manifest, UseColor},
        counterexample,
        heap::{HeapArgs, HeapSettings},
    },
    ExitStatus,
};

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the project
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Run only the tests whose name contains this text
    filter: Option<String>,

    /// Use color in diagnostics
    #[clap(long, value_enum)]
    color: Option<UseColor>,

    /// Leave `tests/generated/` as it is instead of refreshing it from the
    /// verifier's current counterexamples
    #[clap(long)]
    no_counterexample_tests: bool,

    /// Internal: run the single test `--entry` from this library, in this
    /// process, and report the outcome on stdout
    #[clap(long, hide = true, requires = "entry")]
    run_one: Option<PathBuf>,

    /// Internal: the test function `--run-one` calls
    #[clap(long, hide = true)]
    entry: Option<String>,

    #[clap(flatten)]
    heap: HeapArgs,
}

/// Reads the heap settings for the package at `manifest_path`, if there is
/// one.
fn heap_settings(heap: &HeapArgs, manifest_path: Option<&Path>) -> anyhow::Result<HeapSettings> {
    match manifest_path {
        Some(path) => {
            let package = codira_project::Package::from_file(path)?;
            HeapSettings::resolve(heap, Some((package.manifest().runtime(), path)))
        }
        None => HeapSettings::resolve(heap, None),
    }
}

/// The line a `--run-one` child prints so the parent can tell a failed
/// assertion from a crash.
const OUTCOME_PREFIX: &str = "codira-test-outcome:";

pub fn test(args: Args) -> anyhow::Result<ExitStatus> {
    if let (Some(library), Some(entry)) = (&args.run_one, &args.entry) {
        let heap = heap_settings(&args.heap, args.manifest_path.as_deref())?;
        return run_one(library, entry, &heap);
    }

    let manifest_path = locate_manifest(args.manifest_path.as_deref())?;
    let package = codira_project::Package::from_file(&manifest_path)?;
    // Each test resolves the settings again in its process; reading them here
    // reports a bad value once, before the build, and warns once.
    let heap = HeapSettings::resolve(
        &args.heap,
        Some((package.manifest().runtime(), &manifest_path)),
    )?;
    heap.warn_if_unlimited();
    let colors = display_colors(args.color);
    let root = package.root().to_path_buf();

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        // Kept apart from `codira build`'s output: a test build includes the
        // test modules and must not replace the package's real library.
        out_dir: Some(root.join("target").join("test")),
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    };
    let (_, mut driver) = Driver::with_package_path(&manifest_path, config)?;

    if !args.no_counterexample_tests {
        let report = counterexample::sync_generated_tests(
            driver.db(),
            &counterexample::generated_dir(&root),
        )?;
        for path in &report.written {
            println!(
                "wrote counterexample test {}",
                display_relative(&root, path)
            );
        }
        for path in &report.removed {
            println!(
                "removed counterexample test {} (its obligation is proven now)",
                display_relative(&root, path)
            );
        }
    }

    for (virtual_path, contents) in test_sources(&root.join("tests"))? {
        driver.add_file(virtual_path, contents);
    }

    // Unproven contracts are reported but do not stop the run; everything
    // else does.
    let (fatal, unproven) = classify_diagnostics(&driver);
    if fatal + unproven > 0 {
        driver.emit_diagnostics(&mut stderr(), colors)?;
    }
    if fatal > 0 {
        return Ok(ExitStatus::Error);
    }
    if unproven > 0 {
        println!(
            "note: {unproven} contract obligation(s) are not proven; running the tests anyway"
        );
    }

    driver.write_all_assemblies(true)?;

    let db = driver.db();
    let mut tests: Vec<(String, PathBuf)> = Vec::new();
    for package in Package::all(db) {
        for module in package.modules(db) {
            for function in module.all_functions(db) {
                if !function.is_test(db) {
                    continue;
                }
                let name = function.full_name(db);
                if args
                    .filter
                    .as_ref()
                    .is_some_and(|filter| !name.contains(filter.as_str()))
                {
                    continue;
                }
                tests.push((name, driver.assembly_output_path(module)));
            }
        }
    }
    tests.sort();

    println!("\nrunning {} test(s)", tests.len());
    let exe = std::env::current_exe()?;
    let mut failed = Vec::new();
    for (name, library) in &tests {
        let output = Command::new(&exe)
            .arg("test")
            .arg("--run-one")
            .arg(library)
            .arg("--entry")
            .arg(name)
            .arg("--manifest-path")
            .arg(&manifest_path)
            .args(args.heap.forward())
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let outcome = stdout
            .lines()
            .find_map(|line| line.strip_prefix(OUTCOME_PREFIX))
            .map(str::trim);
        let verdict = match outcome {
            Some("pass") if output.status.success() => None,
            Some(other) => Some(other.to_string()),
            None => Some(format!(
                "crashed ({}){}",
                output.status,
                last_line(&String::from_utf8_lossy(&output.stderr))
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default()
            )),
        };
        match verdict {
            None => println!("test {name} ... ok"),
            Some(why) => {
                println!("test {name} ... FAILED: {why}");
                failed.push(name.clone());
            }
        }
    }

    let passed = tests.len() - failed.len();
    println!(
        "\ntest result: {}. {passed} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        failed.len()
    );
    Ok(ExitStatus::from(failed.is_empty()))
}

/// The line of a crashed test's standard error that says why: the runtime's
/// `fatal:` line (running out of memory, which has details after it), or
/// else the last line.
fn last_line(text: &str) -> Option<&str> {
    text.lines()
        .find(|l| l.starts_with("fatal: "))
        .or_else(|| text.lines().rev().find(|l| !l.trim().is_empty()))
}

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// Counts error diagnostics, split into unproven contract obligations and
/// everything else (syntax errors included).
fn classify_diagnostics(driver: &Driver) -> (usize, usize) {
    let db = driver.db();
    let mut fatal = 0;
    let mut unproven = 0;
    for package in Package::all(db) {
        for module in package.modules(db) {
            if let Some(file_id) = module.file_id(db) {
                fatal += codira_hir::AstDatabase::parse(db, file_id).errors().len();
            }
            module.diagnostics(
                db,
                &mut DiagnosticSink::new(|d| {
                    if d.downcast_ref::<ContractViolation>().is_some() {
                        unproven += 1;
                    } else {
                        fatal += 1;
                    }
                }),
            );
        }
    }
    (fatal, unproven)
}

/// Every `.code` file under `tests_dir`, with the source-root path it is
/// compiled at (see the module docs).
fn test_sources(tests_dir: &Path) -> anyhow::Result<Vec<(RelativePathBuf, String)>> {
    let mut out = Vec::new();
    if !tests_dir.is_dir() {
        return Ok(out);
    }
    let generated = counterexample::generated_dir(tests_dir.parent().unwrap_or(tests_dir));
    let mut files = Vec::new();
    collect_code_files(tests_dir, &mut files)?;
    for path in files {
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("could not read '{}': {e}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let module_path = (path.parent() == Some(generated.as_path()))
            .then(|| codira_hir::verify::testgen::module_path_of_file(file_name))
            .flatten();
        let virtual_path = if let Some(segments) = module_path {
            // `math.div_counterexample.code` -> `math/div_counterexample.code`.
            let function_file = file_name
                .rsplit('.')
                .nth(1)
                .map_or_else(|| file_name.to_string(), |stem| format!("{stem}.code"));
            let mut virtual_path = segments.join("/");
            if !virtual_path.is_empty() {
                virtual_path.push('/');
            }
            virtual_path.push_str(&function_file);
            virtual_path
        } else {
            let relative = path.strip_prefix(tests_dir).unwrap_or(&path);
            format!(
                "tests/{}",
                relative.display().to_string().replace('\\', "/")
            )
        };
        out.push((RelativePathBuf::from(virtual_path), contents));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Every `.code` file under `dir`, recursively.
fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_code_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "code") {
            out.push(path);
        }
    }
    Ok(())
}

/// The child side of the runner: load the library, call the test under the
/// fault guard, and print the outcome.
fn run_one(library: &Path, entry: &str, heap: &HeapSettings) -> anyhow::Result<ExitStatus> {
    codira_healing::trap::install();
    // Safety: the library was just compiled by the parent `codira test`.
    let runtime = unsafe { heap.apply(Runtime::builder(library)).finish() }?;
    let definition = runtime
        .get_function_definition(entry)
        .ok_or_else(|| anyhow!("test `{entry}` is not in '{}'", library.display()))?;
    let returns_bool = definition.prototype.signature.return_type.equals::<bool>();

    let outcome = codira_healing::trap::protected(|| -> anyhow::Result<bool> {
        if returns_bool {
            runtime
                .invoke::<bool, ()>(entry, ())
                .map_err(|e| anyhow!("{e}"))
        } else {
            runtime
                .invoke::<(), ()>(entry, ())
                .map_err(|e| anyhow!("{e}"))?;
            Ok(true)
        }
    });
    let (line, ok) = match outcome {
        Ok(Ok(true)) => ("pass".to_string(), true),
        Ok(Ok(false)) => ("returned false".to_string(), false),
        Ok(Err(e)) => (format!("could not be called: {e}"), false),
        Err(fault) => (format!("trapped ({:?})", fault.kind), false),
    };
    println!("{OUTCOME_PREFIX} {line}");
    Ok(ExitStatus::from(ok))
}
