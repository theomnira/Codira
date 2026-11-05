//! Runs the shell tests of the format the host builds programs for. The
//! Mach-O tests drive Apple's toolchain, which only macOS has, and the ELF
//! tests need a toolchain that makes ELF programs, which macOS lacks. On any
//! other host (Windows) there is nothing to run: the tests need a Unix shell
//! and a C compiler targeting ELF, so its one test is listed as ignored and
//! says why.
//!
//! The harness speaks the protocol cargo-nextest uses for a custom test
//! harness: `--list --format terse [--ignored]` lists one test per script
//! and target, `<target>::<script>`, and `<name> --exact` runs one.
// Vendored from mold (see NOTICE) and kept close to upstream, so that mold's changes keep
// applying. The workspace turns on extra lints (.cargo/config.toml) that mold's code does not
// follow, thousands of times over. They are allowed here by name, and nothing else is.
#![allow(
    elided_lifetimes_in_paths,
    clippy::bool_to_int_with_if,
    clippy::cast_lossless,
    clippy::checked_conversions,
    clippy::debug_assert_with_mut_call,
    clippy::default_trait_access,
    clippy::doc_markdown,
    clippy::enum_glob_use,
    clippy::if_not_else,
    clippy::items_after_statements,
    clippy::manual_assert,
    clippy::map_err_ignore,
    clippy::match_same_arms,
    clippy::match_wildcard_for_single_variants,
    clippy::mem_forget,
    clippy::mut_mut,
    clippy::redundant_closure_for_method_calls,
    clippy::rest_pat_in_fully_bound_structs,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::unnested_or_patterns,
    clippy::unused_self,
    clippy::wildcard_imports
)]

#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use std::path::Path;

    #[cfg(target_os = "macos")]
    let (cases, run) = ("tests/macho", funis_tests::macho::run);
    #[cfg(not(target_os = "macos"))]
    let (cases, run) = ("tests/elf", funis_tests::elf::run);

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    run(&root.join(cases), Path::new(env!("CARGO_BIN_EXE_funis")))
}

#[cfg(not(unix))]
fn main() -> std::process::ExitCode {
    funis_tests::run_unsupported(
        "elf::all",
        "the shell tests need a Unix host with a shell and a C compiler that makes ELF programs",
    )
}
