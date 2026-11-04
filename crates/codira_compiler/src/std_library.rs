//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Finds the standard library (`std/` of the Codira distribution) and
//!   works out which of its files a program needs: the modules its
//!   `import std.<module>` lines name, and everything those modules import
//!   in turn, and nothing else.
//!
//! The standard library is a second package next to the program, named
//! `std` (`codira_hir_input::PackageData::name`). It is loaded file by
//! file, only the modules a program's imports reach: most of `std/` does
//! not check yet (`crates/codira_compiler/tests/snapshots/
//! stdlib_check_ratchet__stdlib_check_status.snap`), and loading all of it
//! would give every program those diagnostics. For the same reason the
//! standard library's prelude is not in scope in programs: a program
//! imports what it uses, explicitly. Inside the standard library `root` is
//! the standard library's root, as it always was; `std/prelude.code` is
//! only loaded when something imports it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use codira_hir::PathKind;

/// The environment variable that names the standard library's directory,
/// overriding where the compiler would look for it.
pub const STD_PATH_ENV: &str = "CODIRA_STD_PATH";

/// The name programs reach the standard library by: `import std.fs.fs`.
pub const STD_PACKAGE_NAME: &str = "std";

/// A file every standard-library directory has; how a directory is
/// recognised as one.
const STD_MARKER: &str = "prelude.code";

/// The directory of the standard library this compiler uses, or why there
/// is none.
///
/// In order: the directory `CODIRA_STD_PATH` names; `std/` next to the
/// compiler's executable or one directory above it (an installation,
/// `<prefix>/bin/codira` and `<prefix>/std`); the `std/` of the source tree
/// the compiler was built from, when it still exists (a development build).
pub fn std_library_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(STD_PATH_ENV) {
        let path = PathBuf::from(path);
        return if path.join(STD_MARKER).is_file() {
            Ok(path)
        } else {
            Err(format!(
                "{STD_PATH_ENV} is set to '{}', which is not a standard-library directory \
                 (it has no {STD_MARKER})",
                path.display()
            ))
        };
    }

    let mut candidates = Vec::new();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(exe_dir.join("std"));
        if let Some(prefix) = exe_dir.parent() {
            candidates.push(prefix.join("std"));
        }
    }
    candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std"));

    candidates
        .into_iter()
        .find(|dir| dir.join(STD_MARKER).is_file())
        .map(|dir| dir.canonicalize().unwrap_or(dir))
        .ok_or_else(|| {
            format!(
                "the standard library could not be found: it is not next to the compiler \
                 and the source tree it was built from is gone; set {STD_PATH_ENV} to the \
                 directory that holds std/{STD_MARKER}"
            )
        })
}

/// The module path an import names, as a list of module names from the
/// standard library's root, if the import refers to the standard library.
///
/// From the program, only `std.<..>` does. From a standard-library file in
/// module `current` (its path from the root), `root.<..>` and `std.<..>` are
/// rooted at the standard library, `super.<..>`/`self.<..>` are relative to
/// `current`, and any other path may name a child of `current`.
pub(crate) fn std_module_path(
    path: &codira_hir::Path,
    current: Option<&[String]>,
) -> Option<Vec<String>> {
    let segments: Vec<String> = path.segments.iter().map(ToString::to_string).collect();
    let starts_with_std = segments.first().map(String::as_str) == Some(STD_PACKAGE_NAME);
    match (current, &path.kind) {
        (_, PathKind::Plain) if starts_with_std => Some(segments[1..].to_vec()),
        (None, _) => None,
        (Some(_), PathKind::Package) => Some(segments),
        (Some(current), PathKind::Plain) => Some(current.iter().cloned().chain(segments).collect()),
        (Some(current), PathKind::Super(levels)) => {
            let keep = current.len().checked_sub(usize::from(*levels))?;
            Some(current[..keep].iter().cloned().chain(segments).collect())
        }
    }
}

/// The file that defines the longest prefix of `module_path` that names a
/// module of the standard library in `std_dir`, as a path relative to it
/// (`crypto/hash.code`). An import names a module and then, perhaps, an
/// item in it (`std.crypto.hash.sha256`), so the longest prefix that is a
/// file is the module.
pub(crate) fn std_file_for(std_dir: &Path, module_path: &[String]) -> Option<String> {
    (1..=module_path.len()).rev().find_map(|len| {
        let prefix = module_path[..len].join("/");
        [format!("{prefix}.code"), format!("{prefix}/mod.code")]
            .into_iter()
            .find(|relative| std_dir.join(relative).is_file())
    })
}

/// The module path of a standard-library file from its relative path
/// (`crypto/hash.code` is `crypto.hash`, `a/mod.code` is `a`).
pub(crate) fn module_path_of(relative: &str) -> Vec<String> {
    let without_extension = relative.strip_suffix(".code").unwrap_or(relative);
    let mut segments: Vec<String> = without_extension.split('/').map(str::to_owned).collect();
    if segments.last().map(String::as_str) == Some("mod") {
        segments.pop();
    }
    segments
}

/// The standard-library files a program needs, relative to `std_dir`, with
/// their contents: the closure of `roots` (the module paths the program
/// imports) under the imports of the files themselves.
pub(crate) fn std_closure(
    std_dir: &Path,
    roots: impl IntoIterator<Item = Vec<String>>,
) -> Result<BTreeMap<String, String>, anyhow::Error> {
    let mut files = BTreeMap::new();
    let mut pending: Vec<String> = roots
        .into_iter()
        .filter_map(|path| std_file_for(std_dir, &path))
        .collect();
    while let Some(relative) = pending.pop() {
        if files.contains_key(&relative) {
            continue;
        }
        let full = std_dir.join(&relative);
        let text = std::fs::read_to_string(&full).map_err(|e| {
            anyhow::anyhow!(
                "could not read the standard-library file '{}': {e}",
                full.display()
            )
        })?;
        let current = module_path_of(&relative);
        for import in codira_hir::import_paths_in_source(&text) {
            if let Some(file) = std_module_path(&import, Some(&current))
                .and_then(|path| std_file_for(std_dir, &path))
            {
                if !files.contains_key(&file) {
                    pending.push(file);
                }
            }
        }
        files.insert(relative, text);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path of the only import in `source`.
    fn import(source: &str) -> codira_hir::Path {
        let mut paths = codira_hir::import_paths_in_source(source);
        assert_eq!(paths.len(), 1, "{source}");
        paths.pop().unwrap()
    }

    fn owned(segments: &[&str]) -> Vec<String> {
        segments.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn grouped_imports_are_one_path_per_item() {
        let paths: Vec<String> = codira_hir::import_paths_in_source(
            "import std.crypto.{hash, other as o};\nfunc f() {}",
        )
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(paths, ["std::crypto::hash", "std::crypto::other"]);
    }

    #[test]
    fn a_program_reaches_the_standard_library_only_through_std() {
        assert_eq!(
            std_module_path(&import("import std.crypto.hash;"), None),
            Some(owned(&["crypto", "hash"]))
        );
        assert_eq!(
            std_module_path(&import("import std.crypto.hash.*;"), None),
            Some(owned(&["crypto", "hash"]))
        );
        assert_eq!(std_module_path(&import("import crypto.hash;"), None), None);
        // `root` in a program is the program's root.
        assert_eq!(std_module_path(&import("import root.std.fs;"), None), None);
    }

    #[test]
    fn inside_the_standard_library_root_is_its_root() {
        let current = owned(&["memory", "region"]);
        assert_eq!(
            std_module_path(&import("import root.builtin.types.*;"), Some(&current)),
            Some(owned(&["builtin", "types"]))
        );
        assert_eq!(
            std_module_path(&import("import super.page;"), Some(&current)),
            Some(owned(&["memory", "page"]))
        );
        assert_eq!(
            std_module_path(&import("import std.fs.fs;"), Some(&current)),
            Some(owned(&["fs", "fs"]))
        );
    }

    #[test]
    fn module_paths_of_files() {
        assert_eq!(
            module_path_of("crypto/hash.code"),
            owned(&["crypto", "hash"])
        );
        assert_eq!(module_path_of("a/mod.code"), owned(&["a"]));
    }

    fn std_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std")
    }

    #[test]
    fn the_longest_prefix_that_is_a_file_is_the_module() {
        let std = std_dir();
        assert_eq!(
            std_file_for(&std, &owned(&["crypto", "hash", "sha256_str"])),
            Some("crypto/hash.code".to_owned())
        );
        assert_eq!(
            std_file_for(&std, &owned(&["crypto", "hash"])),
            Some("crypto/hash.code".to_owned())
        );
        assert_eq!(std_file_for(&std, &owned(&["no_such_module"])), None);
    }

    #[test]
    fn the_closure_follows_the_imports_of_standard_library_files() {
        // `ai/tensor/buffer.code` imports `root.ai.tensor.raw.*` and
        // `root.ai.tensor.shape.*`.
        let files = std_closure(&std_dir(), [owned(&["ai", "tensor", "buffer"])]).unwrap();
        let names: Vec<String> = files.keys().cloned().collect();
        assert!(
            names.contains(&"ai/tensor/buffer.code".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"ai/tensor/raw.code".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"ai/tensor/shape.code".to_owned()),
            "{names:?}"
        );
        // Nothing the imports do not reach, and never the prelude.
        assert!(!names.contains(&"prelude.code".to_owned()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("collections/")),
            "{names:?}"
        );
    }
}
