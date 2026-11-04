//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};

mod toml;

/// Contains all information of a package. Usually this information is read from
/// a codira.toml file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    package_id: PackageId,
    metadata: ManifestMetadata,
    eidos: EidosSettings,
    native: NativeSources,
    envelope: Option<EnvelopeTable>,
}

/// The package's semantic envelope, from the manifest's `[envelope]` table
/// (Codira Semantic Envelope version 1):
///
/// ```toml
/// [envelope]
/// allocation = "none"
/// blocking = false
/// foreign = "none"
/// ```
///
/// The keys and values are those of the `@envelope` attribute; a boolean is
/// read as `true`/`false`. This type only holds the entries, in the order
/// TOML gives them: `codira check`, `codira build` and `codira envelope`
/// validate them with the effect analysis (an unknown or unchecked key is an
/// error there) and require every `public` function of the package to fit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvelopeTable {
    /// `(key, value)`, e.g. `("allocation", "none")`.
    pub entries: Vec<(String, String)>,
}

/// C, C++ and assembly sources compiled into the package, from the
/// manifest's `[native]` section:
///
/// ```toml
/// [native]
/// sources = ["c/vec.c", "c/hash.cpp", "c/crc.S"]
/// include = ["c/include"]
/// defines = ["FAST_PATH=1"]
/// flags = ["-O3"]
/// libs = []
/// ```
///
/// Paths are relative to the directory holding `codira.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativeSources {
    /// Source files. `.c` is C, `.cc`/`.cpp`/`.cxx` is C++, and `.s`/`.S`/
    /// `.asm` is assembly (`.S` and `.asm` go through the C preprocessor).
    pub sources: Vec<PathBuf>,
    /// Directories searched for `#include`.
    pub include: Vec<PathBuf>,
    /// Preprocessor definitions, `NAME` or `NAME=VALUE`.
    pub defines: Vec<String>,
    /// Extra flags passed to Clang for every source.
    pub flags: Vec<String>,
    /// Extra libraries to link against, by name.
    pub libs: Vec<String>,
}

impl NativeSources {
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

/// General metadata for a package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestMetadata {
    pub authors: Vec<String>,
}

/// The package's `[eidos]` table: bounds on the mid-level optimizer. A key
/// that is absent is `None`, and the compiler's default applies.
///
/// Every bound counts work, never time; `egraph_timeout_ms` is refused at
/// parse time for that reason (see `toml::TomlEidos`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EidosSettings {
    /// Hard cap on e-nodes per region.
    pub max_egraph_nodes_per_region: Option<usize>,
    /// Rule-application rounds per region.
    pub max_egraph_iterations: Option<usize>,
    /// Exact-extraction search steps per region; `0` extracts greedily.
    pub exact_extraction_budget: Option<u64>,
}

/// Unique identifier of a package and version
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageId {
    name: String,
    version: semver::Version,
}

impl Manifest {
    /// Try to read a manifest from a file
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Manifest, anyhow::Error> {
        // Load the contents of the file
        let file_contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("could not read manifest file: {e}"))?;
        Self::from_str(&file_contents)
    }

    /// Returns the unique identifier of this manifest based on the name and
    /// version
    pub fn package_id(&self) -> &PackageId {
        &self.package_id
    }

    /// Returns the name of the package
    pub fn name(&self) -> &str {
        self.package_id.name()
    }

    /// Returns the version of the package
    pub fn version(&self) -> &semver::Version {
        self.package_id.version()
    }

    /// Returns the metadata information of the package
    pub fn metadata(&self) -> &ManifestMetadata {
        &self.metadata
    }

    /// Returns the package's `[eidos]` settings
    pub fn eidos(&self) -> &EidosSettings {
        &self.eidos
    }

    /// Returns the native (C, C++, assembly) sources of the package
    pub fn native(&self) -> &NativeSources {
        &self.native
    }

    /// Returns the package's `[envelope]` table, if it has one
    pub fn envelope(&self) -> Option<&EnvelopeTable> {
        self.envelope.as_ref()
    }
}

impl PackageId {
    /// Returns the name of the package
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the version of the package
    pub fn version(&self) -> &semver::Version {
        &self.version
    }
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} v{}", self.name(), self.version())
    }
}

impl std::str::FromStr for Manifest {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Parse the contents of the file to toml manifest
        let manifest = ::toml::from_str::<toml::TomlManifest>(s)
            .map_err(|e| anyhow::anyhow!("could not parse manifest: {e}"))?;
        manifest.into_real_manifest()
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::Manifest;

    #[test]
    fn parse() {
        let manifest = Manifest::from_str(
            r#"
        [package]
        name="test"
        version="0.2.0"
        authors = ["Codira Team"]
        "#,
        )
        .unwrap();

        assert_eq!(manifest.name(), "test");
        assert_eq!(
            manifest.version(),
            &semver::Version::from_str("0.2.0").unwrap()
        );
        assert_eq!(manifest.metadata().authors, vec!["Codira Team"]);
        assert_eq!(format!("{}", manifest.package_id()), "test v0.2.0");
        assert_eq!(manifest.eidos(), &crate::EidosSettings::default());
        assert!(manifest.native().is_empty());
    }

    fn with_eidos(table: &str) -> Result<Manifest, anyhow::Error> {
        Manifest::from_str(&format!(
            "[package]\nname = \"test\"\nversion = \"0.2.0\"\n\n[eidos]\n{table}\n"
        ))
    }

    #[test]
    fn eidos_table_sets_the_bounds() {
        let manifest = with_eidos(
            "max_egraph_nodes_per_region = 2500\nmax_egraph_iterations = 4\n\
             exact_extraction_budget = 0",
        )
        .unwrap();
        assert_eq!(
            manifest.eidos(),
            &crate::EidosSettings {
                max_egraph_nodes_per_region: Some(2500),
                max_egraph_iterations: Some(4),
                exact_extraction_budget: Some(0),
            }
        );
    }

    #[test]
    fn egraph_timeout_is_refused_because_it_breaks_determinism() {
        let error = with_eidos("egraph_timeout_ms = 50")
            .unwrap_err()
            .to_string();
        assert!(error.contains("egraph_timeout_ms"), "{error}");
        assert!(error.contains("deterministic"), "{error}");
        assert!(error.contains("max_egraph_nodes_per_region"), "{error}");
    }

    #[test]
    fn ilp_fallback_is_refused_with_the_real_knob() {
        let error = with_eidos("ilp_extractor_fallback = true")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no ILP solver"), "{error}");
        assert!(error.contains("exact_extraction_budget"), "{error}");
    }

    #[test]
    fn eidos_bounds_are_checked() {
        for (table, expected) in [
            ("max_egraph_nodes_per_region = 0", "at least 1"),
            ("max_egraph_nodes_per_region = 50000000", "above the limit"),
            ("max_egraph_iterations = 5000", "above the limit"),
            ("max_egraph_iterations = -1", "could not parse"),
            // A misspelling is an error, not a silently ignored key.
            ("max_egraph_node_per_region = 10", "could not parse"),
        ] {
            let error = with_eidos(table).unwrap_err().to_string();
            assert!(error.contains(expected), "{table}: {error}");
        }
    }

    #[test]
    fn parse_native_sources() {
        let manifest = Manifest::from_str(
            r#"
        [package]
        name="test"
        version="0.2.0"

        [native]
        sources = ["c/add.c", "c/crc.S"]
        include = ["c/include"]
        defines = ["FAST=1"]
        flags = ["-O3"]
        "#,
        )
        .unwrap();

        let native = manifest.native();
        assert_eq!(
            native.sources,
            vec![std::path::PathBuf::from("c/add.c"), "c/crc.S".into()]
        );
        assert_eq!(native.include, vec![std::path::PathBuf::from("c/include")]);
        assert_eq!(native.defines, vec!["FAST=1"]);
        assert_eq!(native.flags, vec!["-O3"]);
        assert!(native.libs.is_empty());
    }

    #[test]
    fn envelope_table_keeps_every_entry_and_reads_booleans() {
        let manifest = Manifest::from_str(
            r#"
        [package]
        name="test"
        version="0.2.0"

        [envelope]
        allocation = "none"
        blocking = false
        colour = "blue"
        "#,
        )
        .unwrap();
        let mut entries = manifest.envelope().unwrap().entries.clone();
        entries.sort();
        // The unknown key is kept, so the effect analysis can reject it by
        // name; it is never dropped here.
        assert_eq!(
            entries,
            vec![
                ("allocation".to_string(), "none".to_string()),
                ("blocking".to_string(), "false".to_string()),
                ("colour".to_string(), "blue".to_string()),
            ]
        );

        let plain = Manifest::from_str("[package]\nname=\"test\"\nversion=\"0.2.0\"\n").unwrap();
        assert!(plain.envelope().is_none());
    }

    #[test]
    fn envelope_values_must_be_strings_or_booleans() {
        let error = Manifest::from_str(
            "[package]\nname=\"test\"\nversion=\"0.2.0\"\n\n[envelope]\nallocation = 0\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("[envelope] allocation"), "{error}");
        assert!(error.contains("string or a boolean"), "{error}");
    }

    #[test]
    fn misspelt_native_key_is_an_error() {
        let error = Manifest::from_str(
            r#"
        [package]
        name="test"
        version="0.2.0"

        [native]
        sources = ["c/add.c"]
        includes = ["c/include"]
        "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("includes"), "{error}");
    }
}
