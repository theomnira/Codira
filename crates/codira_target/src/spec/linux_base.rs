//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use crate::spec::{LinkerFlavor, TargetOptions};

pub fn opts() -> TargetOptions {
    TargetOptions {
        os: "linux".to_string(),
        env: "gnu".to_string(),
        vendor: "unknown".to_string(),
        linker_flavor: LinkerFlavor::Ld,
        ..Default::default()
    }
}
