//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira::{run_with_args, ExitStatus};

/// Main entry point for the `codira` executable.
///
/// Errors are returned, never unwrapped. A bad entry point or an unreadable
/// manifest is an ordinary user mistake, and unwrapping turned every one of
/// them into a Rust panic -- complete with "run with `RUST_BACKTRACE=1`",
/// which invites the user to debug the compiler rather than their own
/// invocation. Returning the error prints the same message as a plain
/// `error:` line and exits non-zero.
fn main() -> Result<(), anyhow::Error> {
    pretty_env_logger::try_init()?;
    let status = run_with_args(std::env::args_os())?;
    match status {
        ExitStatus::Success => {}
        ExitStatus::Error => std::process::exit(1),
    };
    Ok(())
}
