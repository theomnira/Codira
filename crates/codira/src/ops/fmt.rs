//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira fmt`: format `.code` files with the official standard (see
//!   `codira_fmt`), or with `--check` report the files that are not formatted,
//!   for CI.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use codira_fmt::{format_source, line_diff, DiffLine};

use crate::{ops::build::locate_manifest, ExitStatus};

#[derive(clap::Args)]
pub struct Args {
    /// Files or directories to format (directories recursively). Defaults
    /// to every `.code` file of the enclosing package.
    paths: Vec<PathBuf>,

    /// Write nothing; list the files that are not formatted and exit with
    /// an error if there are any
    #[clap(long)]
    check: bool,

    /// With `--check`, also print the changes as a line diff
    #[clap(long, requires = "check")]
    diff: bool,

    /// Format standard input and write the result to standard output
    #[clap(long, conflicts_with_all = ["paths", "check"])]
    stdin: bool,

    /// Path to the manifest of the package to format, when no paths are
    /// given
    #[clap(long, conflicts_with = "paths")]
    manifest_path: Option<PathBuf>,
}

pub fn fmt(args: Args) -> anyhow::Result<ExitStatus> {
    if args.stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        return match format_source(&text) {
            Ok(formatted) => {
                std::io::stdout().write_all(formatted.as_bytes())?;
                Ok(ExitStatus::Success)
            }
            Err(e) => {
                eprintln!("error: <stdin>: {e}");
                Ok(ExitStatus::Error)
            }
        };
    }

    let roots = if args.paths.is_empty() {
        let manifest = locate_manifest(args.manifest_path.as_deref())?;
        vec![manifest
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))]
    } else {
        args.paths.clone()
    };
    let mut files = Vec::new();
    for root in &roots {
        if root.is_file() {
            files.push(root.clone());
        } else if root.is_dir() {
            collect_code_files(root, &mut files)?;
        } else {
            return Err(anyhow!("'{}' does not exist", root.display()));
        }
    }
    files.sort();
    files.dedup();

    let mut errors = 0;
    let mut unformatted = 0;
    let mut changed_lines = 0;
    for file in &files {
        let text = std::fs::read_to_string(file)
            .map_err(|e| anyhow!("could not read '{}': {e}", file.display()))?;
        let formatted = match format_source(&text) {
            Ok(formatted) => formatted,
            Err(e) => {
                eprintln!("error: {}: {e}", file.display());
                errors += 1;
                continue;
            }
        };
        if formatted == text {
            continue;
        }
        unformatted += 1;
        let diff = line_diff(&text, &formatted);
        let removed = diff
            .iter()
            .filter(|l| matches!(l, DiffLine::Removed(_)))
            .count();
        changed_lines += removed;
        if args.check {
            println!("not formatted: {} ({removed} line(s))", file.display());
            if args.diff {
                print_diff(&diff);
            }
        } else {
            std::fs::write(file, formatted)
                .map_err(|e| anyhow!("could not write '{}': {e}", file.display()))?;
            println!("formatted {}", file.display());
        }
    }

    if args.check {
        println!(
            "{} file(s) checked, {unformatted} not formatted ({changed_lines} line(s) would change), {errors} could not be formatted",
            files.len()
        );
        Ok(ExitStatus::from(unformatted == 0 && errors == 0))
    } else {
        println!(
            "{} file(s), {unformatted} reformatted, {errors} could not be formatted",
            files.len()
        );
        Ok(ExitStatus::from(errors == 0))
    }
}

/// Prints the changed lines of a diff with up to two lines of context.
fn print_diff(diff: &[DiffLine<'_>]) {
    const CONTEXT: usize = 2;
    let changed: Vec<bool> = diff
        .iter()
        .map(|l| !matches!(l, DiffLine::Same(_)))
        .collect();
    let mut old_line = 0usize;
    let mut shown_until = 0usize;
    for (i, line) in diff.iter().enumerate() {
        if !matches!(line, DiffLine::Added(_)) {
            old_line += 1;
        }
        let near = changed[i.saturating_sub(CONTEXT)..(i + CONTEXT + 1).min(diff.len())]
            .iter()
            .any(|&c| c);
        if !near {
            continue;
        }
        if i > shown_until || i == 0 {
            println!("@@ line {old_line}");
        }
        shown_until = i + 1;
        match line {
            DiffLine::Same(l) => println!(" {l}"),
            DiffLine::Removed(l) => println!("-{l}"),
            DiffLine::Added(l) => println!("+{l}"),
        }
    }
}

/// Every `.code` file under `dir`, recursively, skipping build output
/// (`target/`) and hidden directories.
pub(crate) fn collect_code_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "target" {
                continue;
            }
            collect_code_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "code") {
            out.push(path);
        }
    }
    Ok(())
}
