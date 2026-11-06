//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use serde_derive::{Deserialize, Serialize};

use super::{
    EidosSettings, EnvelopeTable, Manifest, ManifestMetadata, NativeSources, PackageId,
    RuntimeSettings,
};

/// A manifest as specified in a codira.toml file.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct TomlManifest {
    package: TomlProject,
    eidos: Option<TomlEidos>,
    #[serde(default)]
    native: Option<TomlNative>,
    /// The package's semantic envelope; see [`EnvelopeTable`].
    #[serde(default)]
    envelope: Option<::toml::value::Table>,
    /// How the runtime runs the package; see [`RuntimeSettings`].
    #[serde(default)]
    runtime: Option<TomlRuntime>,
}

/// The `[runtime]` table. Unknown keys are an error: a misspelt
/// `heap-limit` would otherwise run the program under the default limit.
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TomlRuntime {
    heap_limit: Option<::toml::Value>,
    gc_growth: Option<::toml::Value>,
}

impl TomlRuntime {
    /// Keeps each value as text (an integer is written out); what it means
    /// is decided where it is used (`codira run` and the other commands that
    /// start a runtime), with the same rules as their flags.
    fn into_settings(self) -> Result<RuntimeSettings, anyhow::Error> {
        fn text(key: &str, value: Option<::toml::Value>) -> Result<Option<String>, anyhow::Error> {
            match value {
                None => Ok(None),
                Some(::toml::Value::String(text)) => Ok(Some(text)),
                Some(::toml::Value::Integer(n)) => Ok(Some(n.to_string())),
                Some(other) => anyhow::bail!(
                    "[runtime] {key} must be a string or an integer, not {}",
                    other.type_str()
                ),
            }
        }
        Ok(RuntimeSettings {
            heap_limit: text("heap-limit", self.heap_limit)?,
            gc_growth: text("gc-growth", self.gc_growth)?,
        })
    }
}

/// Reads the `[envelope]` table. Every value must be a string or a boolean
/// (`allocation = "none"`, `blocking = false`); anything else is an error.
/// Which keys and values are accepted is decided by the effect analysis
/// (`codira_hir::realtime::envelope`), which rejects an unknown or unchecked
/// key with the same message as for `@envelope`. Nothing is dropped here.
fn envelope_table(table: ::toml::value::Table) -> Result<EnvelopeTable, anyhow::Error> {
    let mut entries = Vec::with_capacity(table.len());
    for (key, value) in table {
        let value = match value {
            ::toml::Value::String(text) => text,
            ::toml::Value::Boolean(flag) => flag.to_string(),
            other => anyhow::bail!(
                "[envelope] {key} must be a string or a boolean (as in `allocation = \"none\"` \
                 or `blocking = false`), not {}",
                other.type_str()
            ),
        };
        entries.push((key, value));
    }
    Ok(EnvelopeTable { entries })
}

/// The `[native]` section: C, C++ and assembly compiled into the package.
///
/// Unknown keys are an error here, unlike elsewhere in the manifest: a
/// misspelt `include` would otherwise compile every source without its
/// headers and fail far from the cause.
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct TomlNative {
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    defines: Vec<String>,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    libs: Vec<String>,
}

/// Represents the `package` section of a codira.toml file.
#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TomlProject {
    name: String,
    version: semver::Version,
    authors: Option<Vec<String>>,
    /// An SPDX license expression, e.g. `"Apache-2.0 OR MIT"`.
    license: Option<String>,
    /// A license text shipped with the package, relative to the manifest.
    #[serde(rename = "license-file")]
    license_file: Option<String>,
}

/// The `[eidos]` table: bounds on the mid-level optimizer.
///
/// Unknown keys are an error rather than ignored: a misspelled bound that
/// silently fell back to its default is exactly the mistake a bound exists
/// to prevent. Two keys are recognized only to be refused with a reason
/// (see `TomlEidos::into_settings`).
#[derive(Deserialize, Serialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct TomlEidos {
    max_egraph_nodes_per_region: Option<u64>,
    max_egraph_iterations: Option<u64>,
    exact_extraction_budget: Option<u64>,
    egraph_timeout_ms: Option<::toml::Value>,
    ilp_extractor_fallback: Option<::toml::Value>,
}

/// The largest e-node cap the manifest accepts. A node costs on the order
/// of a hundred bytes with its hashcons and parent entries, so this is a
/// few hundred megabytes per region -- far past any useful setting, and
/// low enough that a stray extra zero is caught rather than obeyed.
pub const MAX_EGRAPH_NODES_PER_REGION: u64 = 1_000_000;

/// The most rule-application rounds the manifest accepts.
pub const MAX_EGRAPH_ITERATIONS: u64 = 1_000;

impl TomlEidos {
    fn into_settings(self) -> Result<EidosSettings, anyhow::Error> {
        if self.egraph_timeout_ms.is_some() {
            anyhow::bail!(
                "[eidos] egraph_timeout_ms is not supported: Codira's output must be \
                 deterministic, and a wall-clock timeout would make the optimized program \
                 depend on how fast, and how busy, the building machine was. Bound the \
                 e-graph with max_egraph_nodes_per_region and max_egraph_iterations \
                 instead; they count work, so the same sources always build to the same \
                 program"
            );
        }
        if self.ilp_extractor_fallback.is_some() {
            anyhow::bail!(
                "[eidos] ilp_extractor_fallback is not supported: Codira has no ILP solver. \
                 Its exact extractor is a deterministic branch-and-bound search that keeps \
                 the greedy result when its budget runs out; set exact_extraction_budget \
                 (search steps per region, 0 to extract greedily) instead"
            );
        }

        let max_egraph_nodes_per_region = match self.max_egraph_nodes_per_region {
            Some(0) => anyhow::bail!(
                "[eidos] max_egraph_nodes_per_region must be at least 1; to build without \
                 Eidos, use `--mode=dev`"
            ),
            Some(n) if n > MAX_EGRAPH_NODES_PER_REGION => anyhow::bail!(
                "[eidos] max_egraph_nodes_per_region is {n}, above the limit of \
                 {MAX_EGRAPH_NODES_PER_REGION}"
            ),
            other => other,
        };
        let max_egraph_iterations = match self.max_egraph_iterations {
            Some(n) if n > MAX_EGRAPH_ITERATIONS => anyhow::bail!(
                "[eidos] max_egraph_iterations is {n}, above the limit of \
                 {MAX_EGRAPH_ITERATIONS}"
            ),
            other => other,
        };

        // Both bounds were checked against limits far inside `u32`, so the
        // conversions cannot fail on any platform Codira builds for.
        let to_usize = |n: u64| usize::try_from(n).expect("bounded above by a u32 limit");
        Ok(EidosSettings {
            max_egraph_nodes_per_region: max_egraph_nodes_per_region.map(to_usize),
            max_egraph_iterations: max_egraph_iterations.map(to_usize),
            exact_extraction_budget: self.exact_extraction_budget,
        })
    }
}

impl TomlManifest {
    /// Convert this toml manifest into a "real" manifest.
    pub fn into_real_manifest(self) -> Result<Manifest, anyhow::Error> {
        let name = self.package.name.trim();
        if name.is_empty() {
            anyhow::bail!("package name cannot be an empty string");
        }

        Ok(Manifest {
            package_id: PackageId {
                name: name.to_owned(),
                version: self.package.version,
            },
            metadata: ManifestMetadata {
                authors: self.package.authors.unwrap_or_default(),
                license: self.package.license,
                license_file: self.package.license_file.map(Into::into),
            },
            eidos: self.eidos.unwrap_or_default().into_settings()?,
            native: self
                .native
                .map(|native| NativeSources {
                    sources: native.sources.into_iter().map(Into::into).collect(),
                    include: native.include.into_iter().map(Into::into).collect(),
                    defines: native.defines,
                    flags: native.flags,
                    libs: native.libs,
                })
                .unwrap_or_default(),
            envelope: self.envelope.map(envelope_table).transpose()?,
            runtime: self
                .runtime
                .map(TomlRuntime::into_settings)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}
