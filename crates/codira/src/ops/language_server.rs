//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::ExitStatus;

#[derive(clap::Args)]
pub struct Args {}

/// This function is invoked when the executable is invoked with the
/// `language-server` argument. A Codira language server is started ready to
/// serve language information about one or more projects.
pub fn language_server(_args: Args) -> Result<ExitStatus, anyhow::Error> {
    codira_language_server::run_server().map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(ExitStatus::Success)
}
