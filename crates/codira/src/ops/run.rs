//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 13, 2026
//!
//! Functionality:
//! - `codira run`: build the package, then load the library it produced and
//!   invoke its entry point -- `codira build` followed by `codira start`, in
//!   one step.

use std::path::PathBuf;

use codira_compiler::{Config, Target};
use codira_project::Package;

use crate::{
    ops::{
        build::{display_colors, locate_manifest, optimization_level, UseColor},
        heap::{HeapArgs, HeapSettings},
        start::{start_library, StartOptions},
    },
    ExitStatus,
};

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the project
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Optimization level [0,3]
    #[clap(long, short = 'O', default_value_t = 2)]
    opt_level: u8,

    /// Use color in diagnostics
    #[clap(long, value_enum)]
    color: Option<UseColor>,

    /// Run under the HERACLES self-healing supervisor (see `codira start
    /// --heal`)
    #[clap(long)]
    heal: bool,

    /// Build with fuel instrumentation and give the entry point this many
    /// units (one per function entry and loop iteration). Running out ends
    /// the program with a message, or is reported to HERACLES with `--heal`.
    /// Not available with `--jit`, whose backend does not instrument code.
    #[clap(long, conflicts_with = "jit")]
    fuel: Option<u64>,

    /// EXPERIMENTAL: build with frame allocation (see `codira build
    /// --frame-alloc`). Not available with `--jit`.
    #[clap(long, conflicts_with = "jit")]
    frame_alloc: bool,

    /// The function to invoke
    #[clap(default_value_t = String::from("main"))]
    entry: String,

    /// Compile with the Cranelift backend and run in this process: no LLVM
    /// optimization pipeline, no linker. Falls back to the LLVM backend,
    /// saying why, when the program uses something Cranelift does not
    /// compile yet.
    #[clap(long, conflicts_with = "heal")]
    jit: bool,

    #[clap(flatten)]
    heap: HeapArgs,

    /// Print the garbage collector's statistics (heap size and peak,
    /// allocations, collections, pauses) to standard error when the entry
    /// point returns
    #[clap(long)]
    runtime_stats: bool,
}

/// Builds the package `args` names and invokes its entry point.
///
/// Diagnostics are printed exactly as `codira build` prints them; if any is
/// an error nothing is run and the exit status is a failure. The library
/// that is run is the package's root module, `target/mod.codiralib`, which is
/// where `codira build` writes it.
pub fn run(args: Args) -> anyhow::Result<ExitStatus> {
    let manifest_path = locate_manifest(args.manifest_path.as_deref())?;
    let colors = display_colors(args.color);

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: optimization_level(args.opt_level)?,
        out_dir: None,
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    };

    // Read the heap settings first, so a bad value is reported before the
    // build.
    let package = Package::from_file(&manifest_path)?;
    let heap = HeapSettings::resolve(
        &args.heap,
        Some((package.manifest().runtime(), &manifest_path)),
    )?;

    if args.jit {
        if let Some(status) = run_jit(&manifest_path, &config, colors, &args.entry, &heap)? {
            return Ok(status);
        }
    }

    let library = codira_compiler::ensure_package_output_dir(&package, &config)?
        .join("mod")
        .with_extension("codiralib");

    let fuel = args.fuel;
    let frame_alloc = args.frame_alloc;
    if !codira_compiler::compile_manifest_with(&manifest_path, config, colors, |driver| {
        driver.set_fuel_instrumentation(fuel.is_some());
        driver.set_frame_allocation(frame_alloc);
    })? {
        return Ok(ExitStatus::Error);
    }

    heap.warn_if_unlimited();
    start_library(StartOptions {
        library,
        entry: args.entry,
        heal: args.heal,
        fuel: args.fuel,
        heap,
        runtime_stats: args.runtime_stats,
    })
}

/// Runs the package on the Cranelift backend, or returns `None` if it uses
/// something that backend does not compile, in which case the caller runs it
/// on the LLVM backend instead.
fn run_jit(
    manifest_path: &std::path::Path,
    config: &Config,
    colors: codira_compiler::DisplayColor,
    entry: &str,
    heap: &crate::ops::heap::HeapSettings,
) -> anyhow::Result<Option<ExitStatus>> {
    let (_package, driver) =
        codira_compiler::Driver::with_package_path(manifest_path, config.clone())?;
    if let Some(errors) = driver.emit_diagnostics_to_string(colors)? {
        eprint!("{errors}");
        return Ok(Some(ExitStatus::Error));
    }
    let start = std::time::Instant::now();
    // The same heap limit and pacing a runtime started by `codira run` gets.
    let options = codira_cranelift::Options {
        heap_limit: heap.limit,
        heap_limit_note: heap.limit.map(|_| heap.note()),
        gc_policy: Some(codira_runtime::GcPolicy {
            growth_percent: heap.growth,
            ..codira_runtime::GcPolicy::default()
        }),
        ..codira_cranelift::Options::default()
    };
    let program = match codira_cranelift::compile_with(driver.db(), entry, options) {
        Ok(program) => program,
        Err(unsupported) => {
            eprintln!(
                "note: running on the LLVM backend: the Cranelift backend does not compile {unsupported} yet"
            );
            return Ok(None);
        }
    };
    heap.warn_if_unlimited();
    let timings = std::env::var_os("CODIRA_JIT_TIMINGS").is_some();
    if timings {
        eprintln!(
            "cranelift: compiled in {:.3} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    let start = std::time::Instant::now();
    let returned = program.run();
    if timings {
        eprintln!(
            "cranelift: ran in {:.3} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    match returned {
        codira_cranelift::Returned::Unit => {}
        codira_cranelift::Returned::Bool(value) => println!("{value}"),
        codira_cranelift::Returned::Int(value) => println!("{value}"),
        codira_cranelift::Returned::Float(value) => println!("{value}"),
    }
    Ok(Some(ExitStatus::Success))
}
