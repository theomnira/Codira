//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Times each workload of the `.codiralib` named on the command line and
//!   prints one line per workload: `name nanoseconds check` (only those whose
//!   name contains the optional second argument). The worker count comes from
//!   `CODIRA_TASK_WORKERS`, as for any Codira program.

use std::time::Instant;

fn main() {
    let lib = std::env::args()
        .nth(1)
        .expect("usage: fabric_bench_host <library>");
    // Safety: the library is the benchmark program, built by this checkout.
    let runtime = unsafe { codira_runtime::Runtime::builder(lib).finish() }
        .expect("failed to load the library");
    // An optional second argument runs only the workloads it names a part of.
    let only = std::env::args().nth(2).unwrap_or_default();
    let time = |name: &str, f: &dyn Fn() -> i64| {
        if !name.contains(&only) {
            return;
        }
        // One untimed call first: starts the workers, warms the caches.
        f();
        let start = Instant::now();
        let check = f();
        println!("{name} {} {check}", start.elapsed().as_nanos());
    };
    time("spawn_join_100k", &|| {
        runtime.invoke("spawn_join", (100_000i64,)).unwrap()
    });
    time("fan_out_map_1m", &|| {
        runtime
            .invoke("fan_out_map", (1_000_000i64, 64i64))
            .unwrap()
    });
    time("pipeline_1m", &|| {
        runtime.invoke("pipeline", (1_000_000i64,)).unwrap()
    });
    time("compute_64x4m", &|| {
        runtime.invoke("compute", (64i64, 4_000_000i64)).unwrap()
    });
}
