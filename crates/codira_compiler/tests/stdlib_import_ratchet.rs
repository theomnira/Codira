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
//! it does, with the one-line program `import std.<module>.*`:
//!
//! 1. *check*: the program and every standard-library file it reaches have
//!    no diagnostics;
//! 2. *build*: the code generator compiles them all into libraries (each
//!    imported module is compiled whole, whatever the program uses);
//! 3. *load*: the runtime loads the program's library and the standard
//!    library's, resolving every symbol they call, `extern "C"` ones
//!    included.
//!
//! A module is importable when all three pass. Otherwise the snapshot shows
//! the stage that failed and its first error (for a check, in the module
//! itself, or else in the first module it reaches that has one). A module
//! blocked by something the compiler cannot express yet carries the reason
//! in `KNOWN_BLOCKERS`.
//!
//! This says a module can be *used*; it says nothing about whether its
//! functions are right. The end-to-end tests in
//! `crates/codira/tests/std_imports.rs` run them against known values.
//!
//! # Updating
//!
//! Review the diff and accept it with `cargo insta accept`. A module going
//! from importable to blocked is a regression.

use std::{
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Mutex,
};

use codira_compiler::{Config, Driver, OptimizationLevel, Target};

/// Why a module cannot be made importable with the compiler as it is: what
/// is missing, not what is wrong with the module. Only for modules that
/// are blocked; a module that becomes importable must lose its entry.
const KNOWN_BLOCKERS: &[(&str, &str)] = &[
    // What the compiler does not have yet.
    (
        "sys.assert_compare",
        "operators on a type parameter (`==`, `<` through traits) and `String` (`Stringable`)",
    ),
    (
        "hashlib.hasher",
        "in-place parameters: its writers take the hasher as `mut`, which the compiler accepts \
         but passes by copy, so they would change nothing; and `String`",
    ),
    (
        "random.random",
        "module-level mutable state (its generator is a global); and the growable `Array`",
    ),
    (
        "builtin.types",
        "the `f16` and `bf16` types, and sizeof/alignof intrinsics it does not declare",
    ),
    (
        "builtin.traits",
        "generic traits (`Indexable[Index, Element]`, `Iterable[Element]`) and `String`",
    ),
    (
        "memory.pointer",
        "std.builtin.traits and std.builtin.types, `String`, and intrinsics the compiler \
         does not have (`__intrinsic_to_hex`, `__intrinsic_type_name`)",
    ),
    ("collections.list", "`Pointer[T]` (std.memory.pointer)"),
    ("collections.string", "`Pointer[u8]` (std.memory.pointer)"),
    ("collections.deque", "`Pointer[T]` (std.memory.pointer)"),
    (
        "collections.binary_heap",
        "`Pointer[T]` (std.memory.pointer)",
    ),
    ("collections.span", "`Pointer[T]` (std.memory.pointer)"),
    (
        "collections.linked_list",
        "`Pointer[T]` (std.memory.pointer)",
    ),
    ("collections.bitset", "`Pointer[u64]` (std.memory.pointer)"),
    ("collections.set", "`Pointer[T]` (std.memory.pointer)"),
    ("collections.dict", "`Pointer[T]` (std.memory.pointer)"),
    ("collections.counter", "`Dict` (std.collections.dict)"),
    ("math.sequence", "`List` (std.collections.list)"),
    ("math.polynomial", "`List` (std.collections.list)"),
    (
        "base64.base64_string",
        "`String`, the growable `Array[u8]`, and module-level `let`",
    ),
    ("hashlib.string_hash", "`String`"),
    ("complex.complex_string", "`String`"),
    ("format.format", "`String`"),
    ("format.repr", "`String`"),
    (
        "atomic.atomic",
        "module-level `let` (its orderings; the compiler drops them without a word), \
         `String`, and in-place parameters (its operations would act on a copy)",
    ),
];

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

/// Where importing a module stopped, and why.
enum Outcome {
    Importable,
    /// `diagnostics` over the program and every standard-library file
    /// loaded; `first` is `path:line: message`.
    Check {
        diagnostics: usize,
        first: String,
    },
    Build(String),
    Load(String),
}

/// The message of the last panic, recorded by the hook `main` installs: a
/// panic in a code-generation thread reaches the caller only as "a scoped
/// thread panicked".
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn import_outcome(std_dir: &Path, module: &str) -> Outcome {
    let program = tempfile::tempdir().unwrap();
    std::fs::write(
        program.path().join("mod.code"),
        format!("import std.{module}.*\n\npublic func main() {{}}\n"),
    )
    .unwrap();
    let target = program.path().join("target");
    std::fs::create_dir(&target).unwrap();

    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: OptimizationLevel::None,
        mode: codira_compiler::BuildMode::default(),
        out_dir: Some(target.clone()),
        emit_ir: false,
    };
    let mut driver = Driver::with_source_directory(program.path(), config)
        .expect("the program and the standard library load");
    let per_file = driver.collect_diagnostics();

    let diagnostics = per_file.iter().map(|f| f.diagnostics.len()).sum();
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
                if !highlight.is_empty() && !first.contains(&format!("`{highlight}`")) {
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
    if let Some(first) = first {
        return Outcome::Check { diagnostics, first };
    }

    *LAST_PANIC.lock().unwrap() = None;
    match std::panic::catch_unwind(AssertUnwindSafe(|| driver.write_all_assemblies(false))) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Outcome::Build(one_line(&format!("{error:#}"))),
        Err(_) => {
            let message = LAST_PANIC.lock().unwrap().take().unwrap_or_default();
            return Outcome::Build(format!("the code generator panicked: {message}"));
        }
    }

    // Safety: the libraries were compiled by this test, from a program whose
    // `main` is never called; loading runs no Codira code.
    match unsafe { codira_runtime::Runtime::builder(target.join("mod.codiralib")).finish() } {
        Ok(_runtime) => Outcome::Importable,
        Err(error) => Outcome::Load(one_line(&format!("{error:#}"))),
    }
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
    // The driver finds the standard library itself, as for a program; it
    // must be this source tree's.
    assert_eq!(
        codira_compiler::std_library_dir()
            .ok()
            .and_then(|dir| dir.canonicalize().ok()),
        std_dir.canonicalize().ok(),
        "unset {} to run this test against this source tree",
        codira_compiler::STD_PATH_ENV
    );

    // Panics are expected (a module the code generator cannot compile);
    // record the message instead of printing it.
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(ToString::to_string)
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let location = info
            .location()
            .map(|l| format!(" (in {})", l.file()))
            .unwrap_or_default();
        let mut last = LAST_PANIC
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The first panic is the cause; "a scoped thread panicked" follows.
        if last.is_none() {
            *last = Some(format!("{message}{location}"));
        }
    }));

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
        let failure = match outcome {
            Outcome::Importable => {
                assert!(
                    reason.is_none(),
                    "std.{module} is importable; remove it from KNOWN_BLOCKERS"
                );
                importable.push(format!("std.{module}"));
                continue;
            }
            Outcome::Check { diagnostics, first } => format!(
                "check ({diagnostics} diagnostic{}): {first}",
                if diagnostics == 1 { "" } else { "s" }
            ),
            Outcome::Build(error) => format!("build: {error}"),
            Outcome::Load(error) => format!("load: {error}"),
        };
        blocked.push_str(&format!("std.{module}\n    {failure}\n"));
        if let Some(reason) = reason {
            blocked.push_str(&format!("    needs: {reason}\n"));
        }
    }
    let _ = std::panic::take_hook();

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
