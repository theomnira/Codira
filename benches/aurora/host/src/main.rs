//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Runs one kernel of an AURORA benchmark: loads the `.codiralib` named
//!   first on the command line and invokes the public function named second
//!   (it takes nothing and returns `()`; it prints its own `@@ ns=.. check=..`
//!   line), with the runtime compiled in release mode. Up to three more
//!   arguments are passed to it as `i64`s, for kernels that share a function.
//! - Caps the GC heap at `AURORA_HEAP_LIMIT_MB` mebibytes (1024 by default,
//!   `0` for no cap): without a cap the collector never runs.
//! - The collector paces itself with the runtime's default policy (the next
//!   collection at 100% growth over the live data, from 4 MiB), as under
//!   `codira run`.
//! - Afterwards prints one more `@@` line with what the runtime and the
//!   operating system say about memory: `Runtime::stats()` (heap size now
//!   and at its peak, live bytes after the last collection, bytes and
//!   objects allocated, collections by cause, the time the world was
//!   stopped, the time to reach a safepoint, the pause count, median, 99th
//!   percentile and maximum), the peak resident set (`VmHWM`), the limit,
//!   and the time spent loading the library, so that the runner can report
//!   them next to the kernel's time.

use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(lib), Some(entry)) = (args.next(), args.next()) else {
        eprintln!("usage: aurora_host <library> <kernel function> [i64 arguments...]");
        std::process::exit(2);
    };
    let ints: Vec<i64> = args
        .map(|a| a.parse().unwrap_or_else(|_| panic!("`{a}` is not an i64 argument")))
        .collect();
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

    let name = entry.as_str();
    let ran = match *ints.as_slice() {
        [] => runtime.invoke(name, ()).map_err(|e| format!("{e:?}")),
        [a] => runtime.invoke(name, (a,)).map_err(|e| format!("{e:?}")),
        [a, b] => runtime.invoke(name, (a, b)).map_err(|e| format!("{e:?}")),
        [a, b, c] => runtime.invoke(name, (a, b, c)).map_err(|e| format!("{e:?}")),
        _ => panic!("at most three arguments"),
    };
    let () = ran.unwrap_or_else(|e| panic!("failed to run `{entry}`: {e}"));

    let s = runtime.stats();
    let c = &s.collections;
    println!(
        "@@ heap_limit_mb={limit_mb} load_ns={load_ns} gc_heap_bytes={} gc_peak_heap_bytes={} \
         gc_live_after_last_bytes={} gc_bytes_allocated={} gc_objects_allocated={} \
         gc_collections={} gc_collections_paced={} gc_collections_at_limit={} \
         gc_bytes_reclaimed={} gc_collection_time_ns={} gc_time_to_safepoint_ns={} \
         gc_pause_count={} gc_pause_p50_ns={} gc_pause_p99_ns={} gc_pause_max_ns={} {}",
        s.heap_bytes,
        s.peak_heap_bytes,
        s.live_bytes_after_last_collection,
        s.bytes_allocated,
        s.objects_allocated,
        c.paced + c.at_limit + c.requested + c.explicit,
        c.paced,
        c.at_limit,
        s.bytes_reclaimed,
        s.collection_time.as_nanos(),
        s.time_to_safepoint.as_nanos(),
        s.pauses.count,
        s.pauses.p50.as_nanos(),
        s.pauses.p99.as_nanos(),
        s.pauses.max.as_nanos(),
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
