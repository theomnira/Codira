//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::{project_root, update, Mode, Result};

pub const RUNTIME_CAPI_DIR: &str = "crates/codira_runtime_capi";

/// Generates the FFI bindings for the Codira runtime
pub fn generate(mode: Mode) -> Result<()> {
    let crate_dir = project_root().join(RUNTIME_CAPI_DIR);
    let file_path = project_root().join("cpp/include/codira/runtime_capi.h");

    let mut file_contents = Vec::<u8>::new();
    cbindgen::generate(crate_dir)?.write(&mut file_contents);

    let file_contents = String::from_utf8(file_contents)?;
    update(&file_path, &file_contents, mode)
}
