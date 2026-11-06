//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Checks that every runtime symbol the two code generators mention is in
//!   the runtime function catalogue, by reading their sources: a `"codira.*"`
//!   string literal in `codira_codegen` or `codira_cranelift` names either a
//!   catalogued function or one of the few symbols the generated object
//!   defines itself (listed below). A runtime call added without a catalogue
//!   entry fails here even before anything calls it.

use std::path::{Path, PathBuf};

use codira_abi::runtime_functions;

/// `codira.*` names that are not runtime functions: sections and symbols the
/// LLVM backend emits into the object it writes, and the prefixes of the
/// function-value cell entries (see `codira_abi`).
const NOT_RUNTIME_FUNCTIONS: &[&str] = &[
    "codira.init_array",
    "codira.fini_array",
    "codira.dso_fini",
    // The name of the global holding a string literal's bytes.
    "codira.str",
    // Digit tables and the inline decimal writer the String Fabric format
    // path emits into the module.
    "codira.fmt.dec",
    "codira.fmt.pairs",
    "codira.fmt.pow10",
    "codira.fn_cell:",
    "codira.fn_code:",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `"codira.<name>"` literal (a `c"..."` literal too) in `text`.
fn runtime_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("\"codira.") {
        let tail = &rest[at + 1..];
        let end = tail.find('"').unwrap_or(tail.len());
        let name = &tail[..end];
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'))
        {
            names.push(name.to_owned());
        }
        rest = &tail[end.min(tail.len())..];
        if rest.starts_with('"') {
            rest = &rest[1..];
        }
    }
    names
}

#[test]
fn every_runtime_symbol_the_backends_name_is_catalogued() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut unlisted = Vec::new();
    let mut seen = 0;
    for backend in ["codira_codegen", "codira_cranelift"] {
        let mut files = Vec::new();
        rust_files(&crates.join(backend).join("src"), &mut files);
        assert!(!files.is_empty(), "no sources found for `{backend}`");
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for name in runtime_names(&text) {
                seen += 1;
                if runtime_functions::lookup(&name).is_none()
                    && !NOT_RUNTIME_FUNCTIONS.contains(&name.as_str())
                {
                    unlisted.push(format!("{name} ({})", file.display()));
                }
            }
        }
    }
    assert!(seen > 50, "the scan found only {seen} names: is it broken?");
    assert!(
        unlisted.is_empty(),
        "runtime symbols missing from `codira_abi::runtime_functions::CATALOGUE`:\n{}",
        unlisted.join("\n")
    );
}

#[test]
fn the_scan_finds_names() {
    assert_eq!(
        runtime_names(r#"x = "codira.panic"; y = c"codira.dso_fini"; z = "codira. no""#),
        ["codira.panic", "codira.dso_fini"]
    );
}
