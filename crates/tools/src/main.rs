//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use clap::{Parser, Subcommand};
use tools::{Overwrite, Result};

#[derive(Parser)]
#[clap(name = "tasks", version, author)]
struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::enum_variant_names)]
enum Commands {
    /// Generate Rust syntax files
    GenSyntax,

    /// Generate the Codira runtime C API headers
    GenRuntimeCapi,

    /// Generate the Codira ABI headers
    GenAbi,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Commands::GenSyntax => tools::syntax::generate(Overwrite)?,
        Commands::GenAbi => tools::abi::generate(Overwrite)?,
        Commands::GenRuntimeCapi => tools::runtime_capi::generate(Overwrite)?,
    }
    Ok(())
}
