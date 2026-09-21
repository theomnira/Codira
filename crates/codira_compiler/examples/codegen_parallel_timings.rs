// Measures the actual wall-clock effect of module-group-level codegen
// parallelism, in isolation from parsing/inference/diagnostics (which
// `check_phase_timings`, the sibling example, already covers).
//
//     cargo run --release -p codira_compiler --example codegen_parallel_timings
// -- [dir] [iters]
//
// `build_partition` gives every module in a package its own independent
// `ModuleGroup` (own LLVM context, own output), so `assembly_ir` -- the
// query that lowers a group to LLVM IR and runs the optimization pipeline
// on it -- is safe to compute for every group at once. This tool builds a
// *fresh* `Driver` (and therefore an unmemoized Salsa database) for every
// timed run, so neither the serial nor the parallel measurement can ride on
// the other's cached results, then computes every group's `assembly_ir`
// once serially and once via a `db.snapshot()`-per-thread work-stealing
// pool, and reports the ratio.
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use codira_codegen::{CodeGenDatabase, ModuleGroupId};
use codira_compiler::{Config, Driver};
use codira_hir::salsa::ParallelDatabase;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().map_or_else(
        || {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("crates/codira_compiler -> repo root")
                .join("std")
        },
        std::path::PathBuf::from,
    );
    let iterations: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3).max(1);

    let num_threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);

    let mut group_count = 0usize;
    let mut serial_total = Duration::ZERO;
    let mut parallel_total = Duration::ZERO;

    for i in 0..iterations {
        // Serial baseline: a fresh driver, so `assembly_ir` starts unmemoized.
        let driver =
            Driver::with_source_directory(&dir, Config::default()).expect("readable source dir");
        let group_ids: Vec<ModuleGroupId> = driver
            .db()
            .module_partition()
            .iter()
            .map(|(id, _)| id)
            .collect();
        group_count = group_ids.len();

        let start = Instant::now();
        for &group_id in &group_ids {
            std::hint::black_box(driver.db().assembly_ir(group_id));
        }
        let serial_elapsed = start.elapsed();
        serial_total += serial_elapsed;

        // Parallel: a second fresh driver (same source, unmemoized again),
        // computed via one db.snapshot() per worker thread claiming groups
        // off a shared atomic counter -- exactly `Driver`'s own
        // `warm_assemblies_in_parallel`, reproduced here since that helper
        // is private to the crate.
        let driver =
            Driver::with_source_directory(&dir, Config::default()).expect("readable source dir");
        let next_index = AtomicUsize::new(0);

        let start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..num_threads.min(group_ids.len()).max(1) {
                let db = driver.db().snapshot();
                let group_ids = &group_ids;
                let next_index = &next_index;
                scope.spawn(move || loop {
                    let idx = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(&group_id) = group_ids.get(idx) else {
                        break;
                    };
                    std::hint::black_box(db.assembly_ir(group_id));
                });
            }
        });
        let parallel_elapsed = start.elapsed();
        parallel_total += parallel_elapsed;

        println!(
            "iteration {i}: serial {} vs parallel {} ({} groups, {num_threads} threads)",
            format_duration(serial_elapsed),
            format_duration(parallel_elapsed),
            group_ids.len(),
        );
    }

    let serial_mean = serial_total / iterations;
    let parallel_mean = parallel_total / iterations;
    let speedup = serial_mean.as_secs_f64() / parallel_mean.as_secs_f64();

    println!();
    println!("{group_count} module groups, {num_threads} hardware threads available");
    println!("serial mean:   {}", format_duration(serial_mean));
    println!("parallel mean: {}", format_duration(parallel_mean));
    println!("speedup:       {speedup:.2}x");
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
