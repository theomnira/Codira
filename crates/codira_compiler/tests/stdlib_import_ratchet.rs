//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! The standard-library *importability* ratchet.
//!
//! `stdlib_check_ratchet` checks `std/` as one package, with its prelude in
//! scope everywhere. That is not how a program sees the standard library: a
//! program's `import std.<module>` loads that module and the modules it
//! imports, and nothing else -- no prelude (see
//! `codira_compiler::std_library`). A file clean in that ratchet can still
//! be unusable from a program, because it calls a prelude name or imports a
//! module with errors.
//!
//! This test does, for every `std/**/*.code` file, what a program importing
//! it does: it checks a one-line program `import std.<module>.*` and records
//! whether the program checks clean, and if not, the first diagnostic (in
//! the module itself, or else in the first module it reaches that has one).
//! A module that is blocked for a reason the compiler cannot express yet
//! carries that reason in `KNOWN_BLOCKERS`, so the snapshot says why.
//!
//! # Updating
//!
//! Review the diff and accept it with `cargo insta accept`. A module going
//! from importable to blocked is a regression.

use std::path::{Path, PathBuf};

use codira_compiler::{Config, Driver, OptimizationLevel, Target};

/// Why a module cannot be made importable with the compiler as it is: what
/// is missing, not what is wrong with the module. Only for modules that
/// are blocked; a module that becomes importable must lose its entry.
const KNOWN_BLOCKERS: &[(&str, &str)] = &[];

/// The repository root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/codira_compiler -> repo root")
        .to_path_buf()
}

/// Every `.code` file under `dir`, relative to it, with `/` separators,
/// sorted.
fn std_files(dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "code"))
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    files.sort();
    files
}

/// `crypto/hash.code` is `crypto.hash`.
fn module_of(relative: &str) -> String {
    let path = relative.strip_suffix(".code").unwrap_or(relative);
    let path = path.strip_suffix("/mod").unwrap_or(path);
    path.replace('/', ".")
}

/// What importing one module gives a program.
struct Outcome {
    /// Diagnostics over the program and every standard-library file loaded.
    errors: usize,
    /// The first diagnostic: `path:line: message`.
    first: Option<String>,
}

fn import_outcome(std_dir: &Path, module: &str) -> Outcome {
    let program = tempfile::tempdir().unwrap();
    std::fs::write(
        program.path().join("mod.code"),
        format!("import std.{module}.*\n\npublic func main() {{}}\n"),
    )
    .unwrap();

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: OptimizationLevel::None,
        mode: codira_compiler::BuildMode::default(),
        out_dir: None,
        emit_ir: false,
    };
    let driver = Driver::with_source_directory(program.path(), config)
        .expect("the program and the standard library load");
    let per_file = driver.collect_diagnostics();

    let errors = per_file.iter().map(|f| f.diagnostics.len()).sum();
    let own = format!("std/{}", std_file_path(std_dir, module));
    // The module's own first diagnostic, else the first of any file.
    let first = per_file
        .iter()
        .filter(|f| f.relative_path == own)
        .chain(per_file.iter())
        .find_map(|f| {
            f.diagnostics.first().map(|d| {
                let mut first = format!("{}:{}: {}", f.relative_path, d.line, one_line(&d.message));
                let highlight = one_line(&d.highlight);
                if !highlight.is_empty() {
                    let short: String = highlight.chars().take(60).collect();
                    let ellipsis = if short.len() < highlight.len() {
                        "..."
                    } else {
                        ""
                    };
                    first.push_str(&format!(" `{short}{ellipsis}`"));
                }
                first
            })
        });
    Outcome { errors, first }
}

/// The file of `module` relative to `std_dir`.
fn std_file_path(std_dir: &Path, module: &str) -> String {
    let path = module.replace('.', "/");
    if std_dir.join(format!("{path}.code")).is_file() {
        format!("{path}.code")
    } else {
        format!("{path}/mod.code")
    }
}

fn one_line(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn stdlib_import_status_is_stable() {
    let std_dir = repo_root().join("std");
    if !std_dir.is_dir() {
        return;
    }
    // The standard library of this source tree, whatever the environment
    // says. This is the only test in this binary, so nothing races on it.
    std::env::set_var("CODIRA_STD_PATH", &std_dir);

    let files = std_files(&std_dir);
    assert!(!files.is_empty(), "no std/**/*.code files");

    let mut importable = Vec::new();
    let mut blocked = String::new();
    for relative in &files {
        let module = module_of(relative);
        let outcome = import_outcome(&std_dir, &module);
        let reason = KNOWN_BLOCKERS
            .iter()
            .find(|(m, _)| *m == module)
            .map(|(_, r)| *r);
        match outcome.first {
            None => {
                assert!(
                    reason.is_none(),
                    "std.{module} is importable; remove it from KNOWN_BLOCKERS"
                );
                importable.push(format!("std.{module}"));
            }
            Some(first) => {
                blocked.push_str(&format!(
                    "std.{module} ({} diagnostic{})\n    first: {first}\n",
                    outcome.errors,
                    if outcome.errors == 1 { "" } else { "s" },
                ));
                if let Some(reason) = reason {
                    blocked.push_str(&format!("    needs: {reason}\n"));
                }
            }
        }
    }

    let summary = format!(
        "{} of {} std modules are importable from a program\n\nimportable:\n{}\n\nblocked:\n{blocked}",
        importable.len(),
        files.len(),
        importable
            .iter()
            .map(|m| format!("  {m}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    insta::assert_snapshot!("stdlib_import_status", summary);
}
