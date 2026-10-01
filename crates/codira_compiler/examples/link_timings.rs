// Measures what a rebuild costs in a compiler that stays alive, and how much
// of it is the linker:
//
//     cargo run --release -p codira_compiler --example link_timings -- [iters]
//
// `phase_timings`, the sibling example, stops at LLVM IR and says so: object
// emission and linking are "usually larger than everything above". This
// measures exactly that remainder, in the setting where it matters -- a warm
// process (the build daemon, watch mode) rebuilding after an edit.
//
// Each iteration changes one constant in the source and asks the driver to
// write the assembly again, which re-runs the queries the edit invalidated,
// emits the object and links it into a `.codiralib`.
//
// The link mode is whatever `CODIRA_LINK_MODE` selects, so run it twice to
// compare the two on the same machine:
//
//     CODIRA_LINK_MODE=subprocess  ... --example link_timings
//     CODIRA_LINK_MODE=in-process  ... --example link_timings
//
// The first rebuild is reported on its own line because it carries one-time
// costs -- LLVM target initialization, and for an in-process link, paging
// LLD in -- that no later rebuild in the same process pays again.
use std::time::{Duration, Instant};

use codira_compiler::{
    in_process_lld_available, link_mode, Config, Driver, LinkMode, PathOrInline,
};

/// One function whose body changes on every iteration, so each rebuild has
/// real work to do instead of hitting the salsa cache.
fn source(n: u32) -> String {
    format!("public func main() -> i64 {{\n    {n}\n}}\n")
}

fn main() {
    let iterations: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .max(2);

    let out_dir = tempfile::tempdir().expect("temporary output directory");

    let label = match link_mode() {
        LinkMode::InProcess if in_process_lld_available() => "in-process LLD",
        LinkMode::InProcess => "spawned linker (built without `in-process-lld`)",
        LinkMode::Subprocess => "spawned linker",
    };

    let config = Config {
        out_dir: Some(out_dir.path().to_path_buf()),
        ..Config::default()
    };
    let (mut driver, _) = Driver::with_file(
        config,
        PathOrInline::Inline {
            rel_path: codira_paths::RelativePathBuf::from("mod.code"),
            contents: source(0),
        },
    )
    .expect("driver for the sample");

    let mut timings = Vec::with_capacity(iterations as usize);
    for i in 0..iterations {
        driver.update_file("mod.code", source(i + 1));

        let start = Instant::now();
        driver
            .write_all_assemblies(false)
            .expect("the sample compiles and links");
        timings.push(start.elapsed());
    }

    let first = timings[0];
    let mut rest = timings[1..].to_vec();
    rest.sort();
    let median = rest[rest.len() / 2];
    let mean = rest.iter().sum::<Duration>() / rest.len() as u32;

    println!("{label}");
    println!("  first rebuild:    {}", format_duration(first));
    println!(
        "  later rebuilds:   median {}, mean {}, min {}, max {}  ({} runs)",
        format_duration(median),
        format_duration(mean),
        format_duration(rest[0]),
        format_duration(rest[rest.len() - 1]),
        rest.len()
    );
}

/// Formats a duration in whichever unit keeps it readable.
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
