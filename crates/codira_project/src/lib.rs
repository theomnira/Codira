//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
pub use manifest::{Manifest, ManifestMetadata, PackageId};
pub use package::Package;
pub use project_manifest::ProjectManifest;

mod manifest;
mod package;
mod project_manifest;

pub const MANIFEST_FILENAME: &str = "codira.toml";
pub const LOCKFILE_NAME: &str = ".codiralock";
