//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{path::Path, str::FromStr};

use codira_project::{Manifest, Package};
use semver::Version;

#[test]
fn manifest_from_file() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/codira.toml");
    let manifest = Manifest::from_file(manifest_path).expect("could not load manifest");
    assert_eq!(manifest.metadata().authors, vec!["Codira Team"]);
    assert_eq!(manifest.version(), &Version::from_str("0.2.0").unwrap());
    assert_eq!(manifest.name(), "test");
}

#[test]
fn package_from_file() {
    let manifest_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/codira.toml");
    let package = Package::from_file(&manifest_path).expect("could not load package");
    assert_eq!(package.name(), "test");
    assert_eq!(package.version(), &Version::from_str("0.2.0").unwrap());
    assert_eq!(package.manifest().metadata().authors, vec!["Codira Team"]);
    assert_eq!(package.manifest_path(), &manifest_path);
    assert_eq!(&package.root(), &manifest_path.parent().unwrap());
    assert_eq!(format!("{package}"), "test v0.2.0");

    let source_dir = package.source_directory();
    assert_eq!(source_dir, manifest_path.parent().unwrap().join("src"));
}
