//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
mod ops;

use std::ffi::OsString;

use clap::{Parser, Subcommand};
/// Standalone executables: what `codira build --exe` writes and what the
/// `codira-launcher` binary runs.
pub use ops::exe;
use ops::{
    api_diff, attest, bindgen, build, certify, check, daemon, doc, envelope, fmt, import_c, init,
    language_server, new, quantum, query, run, start, synth, test,
};

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Run the Codira language server
    LanguageServer(language_server::Args),

    /// Compiles a local Codira file into a module
    Build(build::Args),

    /// Compares the public API of two versions of a package (directories or
    /// git revisions) and says whether each change is major, minor or patch;
    /// `--check` fails when the manifest's version is not bumped far enough
    ApiDiff(api_diff::Args),

    /// Checks the proof certificates of a library, directory or executable
    /// built with `--certify`, without the compiler's front end
    Certify(certify::Args),

    /// Type-checks Codira sources and reports diagnostics, emitting nothing
    Check(check::Args),

    /// Format `.code` files with the official standard; `--check` lists
    /// the files that are not formatted (for CI)
    Fmt(fmt::Args),

    /// Generate static, searchable HTML API documentation for the package
    /// (or `--std`), with each function's inferred envelope
    Doc(doc::Args),

    /// Run or control the build daemon, which keeps a compiler warm between
    /// builds
    Daemon(daemon::Args),

    /// Generate FFI bindings for this project's `@export("C")` functions
    Bindgen(bindgen::Args),

    /// Create a new Codira project at the specified location
    New(new::Args),

    /// Initialize a new Codira project in the specified location
    Init(init::Args),

    /// Invoke a function from a codiralib
    Start(start::Args),

    /// Build the package, then invoke its entry point (`main` by default)
    Run(run::Args),

    /// Quantum kernels: build to QIR/OpenQASM, simulate, import circuits
    Quantum(quantum::Args),

    /// Zero-knowledge proofs of `oblivious` functions: build the circuit and
    /// keys, prove a result on private inputs, verify a proof
    Attest(attest::Args),

    /// Build the package with its `tests/` and run every `@test` function,
    /// each in its own process
    Test(test::Args),

    /// Find the functions that can satisfy a predicate over `result` and
    /// their parameters (e.g. "result < 0"), with a witness input for each
    Query(query::Args),

    /// Every function's semantic envelope (what it may allocate, block on,
    /// panic, run, spawn, perform or touch that is secret) as the compiler
    /// infers it, beside its `@envelope` declaration (experimental)
    Envelope(envelope::Args),

    /// Read a C header with Clang and write Codira `extern "C"` declarations
    /// for its functions
    ImportC(import_c::Args),

    /// Fill `??` holes and `choose` expressions with expressions proven to
    /// meet the contract, by search or by asking an external `--oracle`
    Synth(synth::Args),
}

#[derive(Copy, Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    Success,
    Error,
}

impl From<bool> for ExitStatus {
    fn from(value: bool) -> Self {
        if value {
            ExitStatus::Success
        } else {
            ExitStatus::Error
        }
    }
}

pub fn run_with_args<T, I>(args: I) -> Result<ExitStatus, anyhow::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = Args::parse_from(args);
    match args.command {
        Command::Build(args) => build::build(args),
        Command::ApiDiff(args) => api_diff::api_diff(args),
        Command::Certify(args) => certify::certify(args),
        Command::Check(args) => check::check(args),
        Command::Daemon(args) => daemon::daemon(args),
        Command::Fmt(args) => fmt::fmt(args),
        Command::Doc(args) => doc::doc(args),
        Command::Bindgen(args) => bindgen::bindgen(args),
        Command::LanguageServer(args) => language_server::language_server(args),
        Command::New(args) => new::new(args),
        Command::Init(args) => init::init(args),
        Command::Start(args) => start::start(args),
        Command::Run(args) => run::run(args),
        Command::Quantum(args) => quantum::quantum(args),
        Command::Attest(args) => attest::attest(args),
        Command::Test(args) => test::test(args),
        Command::Query(args) => query::query(args),
        Command::Envelope(args) => envelope::envelope(args),
        Command::Synth(args) => synth::synth(args),
        Command::ImportC(args) => import_c::import_c(args),
    }
}
