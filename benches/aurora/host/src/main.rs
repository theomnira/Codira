//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Runs one kernel of an AURORA benchmark: loads the `.codiralib` named
//!   first on the command line and invokes the public function named second
//!   (it takes nothing and returns `()`; it prints its own `@@ ns=.. check=..`
//!   line), with the runtime compiled in release mode.
//! - Caps the GC heap at `AURORA_HEAP_LIMIT_MB` mebibytes (1024 by default,
//!   `0` for no cap): without a cap the collector never runs.
//! - Afterwards prints one more `@@` line with what the runtime and the
//!   operating system say about memory: the GC heap's allocated bytes, the
//!   peak resident set (`VmHWM`), the limit, and the time spent loading the
//!   library, so that the runner can report them next to the kernel's time.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(lib), Some(entry)) = (args.next(), args.next()) else {
        eprintln!("usage: aurora_host <library> <kernel function>");
        std::process::exit(2);
    };
    let limit_mb: usize = std::env::var("AURORA_HEAP_LIMIT_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);

    let load_start = Instant::now();
    let mut builder = codira_runtime::Runtime::builder(lib);
    if limit_mb > 0 {
        builder = builder.heap_limit(limit_mb << 20);
    }
    // Safety: the library is a benchmark program built from this checkout.
    let runtime = unsafe { builder.finish() }.expect("failed to load the library");
    let load_ns = load_start.elapsed().as_nanos();

    let () = runtime
        .invoke(entry.as_str(), ())
        .unwrap_or_else(|e| panic!("failed to run `{entry}`: {e:?}"));

    let stats = runtime.gc_stats();
    println!(
        "@@ gc_allocated_bytes={} heap_limit_mb={} load_ns={} {}",
        stats.allocated_memory,
        limit_mb,
        load_ns,
        peak_rss()
    );
}

/// `rss_peak_kb=<n>` from `/proc/self/status`, or nothing where there is no
/// such file (the runner then falls back to `wait4`'s `ru_maxrss`).
fn peak_rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("VmHWM:"))
                .map(|rest| rest.trim().trim_end_matches("kB").trim().to_string())
        })
        .map(|kb| format!("rss_peak_kb={kb}"))
        .unwrap_or_default()
}
