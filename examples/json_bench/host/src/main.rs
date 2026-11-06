//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Runs `main` of the `.codiralib` named on the command line with the GC
//!   heap capped at `CODIRA_HEAP_LIMIT_MB` mebibytes (1024 by default), so
//!   that the collector runs when the cap is reached (see
//!   `RuntimeBuilder::heap_limit`).

fn main() {
    let lib = std::env::args()
        .nth(1)
        .expect("usage: json_bench_host <library>");
    let limit_mb: usize = std::env::var("CODIRA_HEAP_LIMIT_MB")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1024);
    // Safety: the library is the benchmark program, built by this checkout.
    let runtime = unsafe {
        codira_runtime::Runtime::builder(lib)
            .heap_limit(limit_mb << 20)
            .finish()
    }
    .expect("failed to load the library");
    let () = runtime.invoke("main", ()).expect("failed to run `main`");
}
