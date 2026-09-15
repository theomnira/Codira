//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira::{run_with_args, ExitStatus};

/// Main entry point for the `codira` executable.
fn main() -> Result<(), anyhow::Error> {
    pretty_env_logger::try_init()?;
    let status = run_with_args(std::env::args_os()).unwrap();
    match status {
        ExitStatus::Success => {}
        ExitStatus::Error => std::process::exit(1),
    };
    Ok(())
}
