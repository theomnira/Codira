//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//! - Emits the compiler host target triple as a build-time environment variable
//!   consumed by `codira_target`.
fn main() {
    println!(
        "cargo:rustc-env=CFG_COMPILER_HOST_TRIPLE={}",
        std::env::var("TARGET").unwrap()
    );
}
