//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Formats every `.code` file in the repository and checks that the result is
//!   idempotent and parses to the same tree.

use std::path::{Path, PathBuf};

use codira_fmt::{check_equivalent, format_source, line_diff, DiffLine, FormatError};

fn code_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "target" || name == "lab" {
                continue;
            }
            code_files(&path, out);
        } else if name.ends_with(".code") {
            out.push(path);
        }
    }
}

#[test]
fn every_repository_source_formats_idempotently_and_equivalently() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    code_files(&root, &mut files);
    files.sort();
    assert!(files.len() > 100, "found only {} .code files", files.len());

    let mut formatted = 0;
    let mut syntax_errors = Vec::new();
    let mut failures = Vec::new();
    let mut changed_files = 0;
    let mut changed_lines = 0;
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        match format_source(&text) {
            Ok(once) => {
                formatted += 1;
                match format_source(&once) {
                    Ok(twice) if twice == once => {}
                    Ok(_) => failures.push(format!("{}: not idempotent", file.display())),
                    Err(e) => failures.push(format!("{}: second pass: {e}", file.display())),
                }
                if let Err(e) = check_equivalent(&text, &once) {
                    failures.push(format!("{}: {e}", file.display()));
                }
                if once != text {
                    changed_files += 1;
                    changed_lines += line_diff(&text, &once)
                        .iter()
                        .filter(|l| matches!(l, DiffLine::Removed(_)))
                        .count();
                }
            }
            Err(FormatError::Syntax(_)) => syntax_errors.push(file.display().to_string()),
            Err(e) => failures.push(format!("{}: {e}", file.display())),
        }
    }
    eprintln!(
        "{} files: {formatted} formatted ({changed_files} would change, {changed_lines} lines), \
         {} skipped for syntax errors",
        files.len(),
        syntax_errors.len()
    );
    for file in &syntax_errors {
        eprintln!("  syntax errors: {file}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
