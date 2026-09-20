//! Copyright (c) 2026 Omnira CJSC
//! Date: September 20, 2026
//!
//! The stdlib *semantic* ratchet.
//!
//! `codira_syntax`'s `stdlib_ratchet` measures whether `std/**/*.code` can be
//! parsed. Parsing is the cheap half: a file whose every name is unresolved
//! and whose every type is wrong still parses perfectly. This test runs the
//! same files through the real frontend -- imports, name resolution, type
//! inference -- which is the half that says whether the standard library
//! actually *means* anything to the compiler.
//!
//! # Why counts per file and not a pass/fail total
//!
//! At the time this was written, 1 of 74 files was clean and there were
//! 5019 diagnostics; a pass/fail assertion would have been red for a very
//! long time while saying nothing about progress, and a bare total would
//! let one file's improvement hide another's regression. Per-file counts
//! make both directions visible in the snapshot diff.
//!
//! # Updating
//!
//! Review the diff and accept it with `cargo insta accept`. Reading the
//! diff is the point: a file whose count goes *up* is a regression that
//! this test exists to catch.

use std::path::{Path, PathBuf};

use codira_compiler::{Config, DiagnosticKind, Driver, OptimizationLevel, Target};

/// The repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/codira_compiler -> repo root")
        .to_path_buf()
}

#[test]
fn stdlib_check_status_is_stable() {
    let root = repo_root();
    let std_dir = root.join("std");
    if !std_dir.is_dir() {
        // The stdlib is not present in every checkout layout; skipping is
        // better than a confusing failure.
        return;
    }

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
    };

    let driver =
        Driver::with_source_directory(&std_dir, config).expect("std/ is a readable source tree");
    let per_file = driver.collect_diagnostics();

    assert!(!per_file.is_empty(), "no std/**/*.code files were checked");

    let clean = per_file.iter().filter(|f| f.is_clean()).count();
    let syntax: usize = per_file
        .iter()
        .map(|f| f.count_of(DiagnosticKind::Syntax))
        .sum();
    let semantic: usize = per_file
        .iter()
        .map(|f| f.count_of(DiagnosticKind::Semantic))
        .sum();

    let mut report = String::new();
    for file in &per_file {
        report.push_str(&format!(
            "{:>4} syntax {:>5} semantic  {}\n",
            file.count_of(DiagnosticKind::Syntax),
            file.count_of(DiagnosticKind::Semantic),
            file.relative_path,
        ));
    }

    let summary = format!(
        "{clean} of {} std files check clean\n{syntax} syntax, {semantic} semantic\n\n{report}",
        per_file.len(),
    );
    insta::assert_snapshot!("stdlib_check_status", summary);
}
