// Measures where `codira check`-shaped work goes, across a whole directory of
// sources, phase by phase:
//
//     cargo run --release -p codira_compiler --example check_phase_timings --
// [dir] [iters]
//
// `phase_timings` (the sibling example) measures compiling *one* small file
// through to LLVM IR. It answers "how fast is codegen". This answers a
// different question -- "how fast is checking a real package" -- which is
// what `codira check` and an IDE's on-type diagnostics actually do: parse,
// resolve, and infer every file, with no codegen at all.
//
// # Why a separate tool rather than extending `phase_timings`
//
// `phase_timings` intentionally recreates its driver from a fresh in-memory
// string every iteration, which is right for isolating front-end phases on a
// toy input but wrong here: rereading dozens of files from disk 50 times to
// measure a directory would make disk I/O the dominant and misleading cost.
// This tool reads the directory once and reuses that driver across the
// timed phases, matching what `codira check` itself does in one process.
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use codira_compiler::{Config, DisplayColor, Driver};
use codira_hir::{AstDatabase, DefDatabase, Module, Package};

struct Phase {
    name: &'static str,
    elapsed: Duration,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().map_or_else(
        || {
            // Defaults to this checkout's own standard library, which at 77
            // files and ~21k lines is the only sizeable body of real Codira
            // source in the repository -- an actual workload, not a
            // synthetic one.
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crates/codira_compiler -> repo root")
                .join("std")
        },
        PathBuf::from,
    );
    let iterations: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5).max(1);

    let file_count = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "code"))
        .count();
    println!(
        "input: {} ({file_count} files), {iterations} iterations\n",
        dir.display()
    );

    let mut parse_total = Duration::ZERO;
    let mut item_tree_total = Duration::ZERO;
    let mut name_res_total = Duration::ZERO;
    let mut infer_total = Duration::ZERO;
    let mut diagnostics_total = Duration::ZERO;
    let mut emit_total = Duration::ZERO;
    let mut load_total = Duration::ZERO;
    let mut whole_total = Duration::ZERO;

    let mut function_count = 0usize;
    let mut diagnostic_count = 0usize;

    for _ in 0..iterations {
        let load_start = Instant::now();
        let driver =
            Driver::with_source_directory(&dir, Config::default()).expect("readable source dir");
        load_total += load_start.elapsed();

        let whole_start = Instant::now();

        // Parse every file. `Package::all` is itself cheap (it reads a
        // salsa input, not a query), so building the module list first and
        // timing the parse of each file separately is what isolates parsing
        // from name resolution below, which also needs the module list.
        let packages = Package::all(driver.db());
        let modules: Vec<Module> = packages
            .iter()
            .flat_map(|package| package.modules(driver.db()))
            .collect();
        let file_ids: Vec<_> = modules
            .iter()
            .filter_map(|m| m.file_id(driver.db()))
            .collect();

        let start = Instant::now();
        for &file_id in &file_ids {
            std::hint::black_box(driver.db().parse(file_id));
        }
        parse_total += start.elapsed();

        let start = Instant::now();
        for &file_id in &file_ids {
            std::hint::black_box(driver.db().item_tree(file_id));
        }
        item_tree_total += start.elapsed();

        // Name resolution: package_defs resolves every import to a fixed
        // point and builds each module's item scope. Asking any module for
        // its scope forces the whole package's resolution, so one call
        // suffices to charge this phase honestly.
        let start = Instant::now();
        if let Some(package) = packages.first() {
            std::hint::black_box(package.modules(driver.db()));
        }
        name_res_total += start.elapsed();

        let start = Instant::now();
        let functions: Vec<_> = modules
            .iter()
            .flat_map(|module| module.all_functions(driver.db()))
            .collect();
        for function in &functions {
            std::hint::black_box(function.infer(driver.db()));
        }
        infer_total += start.elapsed();
        function_count = functions.len();

        // Diagnostic collection: what `codira check` actually asks for once
        // inference is done -- walking every function's inference result
        // into rendered diagnostics.
        let start = Instant::now();
        let diagnostics = driver.collect_diagnostics();
        diagnostic_count = diagnostics.iter().map(|f| f.diagnostics.len()).sum();
        std::hint::black_box(&diagnostics);
        diagnostics_total += start.elapsed();

        // `codira check`'s human-readable path: rendering every diagnostic
        // through `annotate_snippets`, into an in-memory sink so this is
        // purely the rendering cost, with no terminal or redirect involved.
        let start = Instant::now();
        let mut sink = Vec::new();
        driver
            .emit_diagnostics(&mut sink, DisplayColor::Disable)
            .expect("rendering diagnostics");
        std::hint::black_box(&sink);
        emit_total += start.elapsed();

        whole_total += whole_start.elapsed();
    }

    let phases = [
        Phase {
            name: "parse",
            elapsed: parse_total / iterations,
        },
        Phase {
            name: "item tree",
            elapsed: item_tree_total / iterations,
        },
        Phase {
            name: "name resolution",
            elapsed: name_res_total / iterations,
        },
        Phase {
            name: "type inference",
            elapsed: infer_total / iterations,
        },
        Phase {
            name: "collect diagnostics",
            elapsed: diagnostics_total / iterations,
        },
        Phase {
            name: "emit diagnostics (render)",
            elapsed: emit_total / iterations,
        },
    ];
    let total = whole_total / iterations;

    println!("{function_count} functions, {diagnostic_count} diagnostics on the final iteration\n");
    println!("{:<20} {:>12}  {:>7}", "phase", "mean", "share");
    println!("{}", "-".repeat(42));
    for phase in &phases {
        let share = if total.as_nanos() == 0 {
            0.0
        } else {
            100.0 * phase.elapsed.as_nanos() as f64 / total.as_nanos() as f64
        };
        println!(
            "{:<20} {:>12}  {:>6.1}%",
            phase.name,
            format_duration(phase.elapsed),
            share
        );
    }
    println!("{}", "-".repeat(42));
    println!("{:<20} {:>12}", "checked total", format_duration(total));
    println!(
        "{:<20} {:>12}  (directory scan + reading every file from disk)",
        "load (excluded)",
        format_duration(load_total / iterations)
    );
}

fn format_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    if nanos < 1_000 {
        format!("{nanos} ns")
    } else if nanos < 1_000_000 {
        format!("{:.2} us", nanos as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", nanos as f64 / 1_000_000.0)
    }
}
