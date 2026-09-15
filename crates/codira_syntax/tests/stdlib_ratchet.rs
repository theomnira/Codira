//! Copyright (c) 2026 Omnira CJSC
//!
//! The stdlib parse ratchet.
//!
//! `std/**/*.code` is Codira's own standard library, and the files that
//! fail to parse *are* the missing-language-feature list. This test turns
//! that into a measurement that cannot silently regress.
//!
//! # Why a per-file snapshot and not a count
//!
//! A bare "N of 65 parse" assertion can hide a regression behind a fix: if
//! milestone X makes file A parse and breaks file B, the total is
//! unchanged and the test stays green. The snapshot below records the
//! status of **every file individually**, so a file moving from `ok` to
//! `FAIL` is visible in the diff even when the total does not move.
//!
//! # Updating
//!
//! When a milestone legitimately unblocks files, review the diff and
//! accept it with `cargo insta accept`. The review step is the point: an
//! unexpected `ok -> FAIL` transition in that diff is a regression caught
//! before it lands, and the ratchet is only meaningful if the diff is
//! actually read.
//!
//! See `spec/EIDOS_RFC_002.md` section 3 for the failure matrix this
//! drives, and `KGEN_SUPERSET_STATUS.md` for the milestone history.

use std::path::{Path, PathBuf};

/// Walks `dir` collecting every `.code` file, sorted for determinism.
fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_code_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "code") {
            out.push(path);
        }
    }
}

/// The repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/codira_syntax -> repo root")
        .to_path_buf()
}

#[test]
fn stdlib_parse_status_is_stable() {
    let root = repo_root();
    let std_dir = root.join("std");
    if !std_dir.is_dir() {
        // The stdlib is not present in every checkout layout; skipping is
        // better than a confusing failure.
        return;
    }

    let mut files = Vec::new();
    collect_code_files(&std_dir, &mut files);
    assert!(!files.is_empty(), "no std/**/*.code files found");

    let mut ok = 0usize;
    let mut report = String::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("readable source file");
        let parse = codira_syntax::SourceFile::parse(&source);
        let errors = parse.errors();

        let relative = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        if errors.is_empty() {
            ok += 1;
            report.push_str(&format!("ok    {relative}\n"));
        } else {
            // The error count is included because it is a finer-grained
            // signal than pass/fail: a milestone that halves a file's
            // errors without reaching zero is still visible progress.
            report.push_str(&format!("FAIL  {relative}  ({} errors)\n", errors.len()));
        }
    }

    let summary = format!("{ok} of {} std files parse\n\n{report}", files.len());
    insta::assert_snapshot!("stdlib_parse_status", summary);
}
