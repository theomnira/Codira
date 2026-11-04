//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - `codira certify <artifact>`: checks the proof certificates `codira build
//!   --certify` wrote, using only `codira_certificate` and Z3 -- none of the
//!   parser, type checker, verifier or code generator runs.
//!
//! The artifact is a `.codiralib` (its certificate is the
//! `<library>.cert.json` beside it), a directory of them, or an executable
//! from `codira build --exe --certify`, whose payload carries both.
//!
//! One step is the exception to "no front end": a `codira-certificate/2`
//! records each function's inferred semantic envelope, and re-deriving it
//! needs the effect analysis. When the package's source can be found
//! (`--source-dir`, `--manifest-path`, or the `codira.toml` in the
//! artifact's directory or a parent), its envelopes are computed from it and
//! any difference from the recorded ones fails the certificate. When it
//! cannot be found, the report says the envelopes were not re-derived.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use codira_certificate::{Certificate, CheckOptions, DerivedEnvelope, Report, SIDECAR_SUFFIX};
use codira_compiler::{Config, Driver, Target};

use crate::ExitStatus;

#[derive(clap::Args)]
pub struct Args {
    /// A `.codiralib`, a directory of them, or an executable built with
    /// `codira build --exe --certify`
    artifact: PathBuf,

    /// The Z3 resource budget every query is re-decided under
    #[clap(long, default_value_t = default_rlimit())]
    rlimit: u32,

    /// The package's source directory (`src/`): also confirm each
    /// function's source text is the one the certificate was made from
    #[clap(long)]
    source_dir: Option<PathBuf>,

    /// The package's manifest, whose source the recorded semantic envelopes
    /// are re-derived from (default: `--source-dir`, or the `codira.toml` in
    /// the artifact's directory or a parent)
    #[clap(long, conflicts_with = "source_dir")]
    manifest_path: Option<PathBuf>,

    /// Fail when the certificate records semantic envelopes that could not
    /// be re-derived (by default that is reported but is not a failure)
    #[clap(long)]
    require_envelopes: bool,

    /// Fail when the certificate records an obligation as not proven (by
    /// default those are reported but are not a failure: the compiler
    /// accepted them because a run-time check enforces them)
    #[clap(long)]
    deny_unproven: bool,
}

fn default_rlimit() -> u32 {
    codira_certificate::CheckOptions::default().rlimit
}

/// A library and the text of its certificate, if it has one.
struct Certified {
    name: String,
    library: Vec<u8>,
    certificate: Option<Vec<u8>>,
}

fn sidecar(library: &Path) -> anyhow::Result<Certified> {
    let certificate_path = codira_certificate::sidecar_path(library);
    Ok(Certified {
        name: library.display().to_string(),
        library: std::fs::read(library)
            .with_context(|| format!("could not read '{}'", library.display()))?,
        certificate: std::fs::read(&certificate_path).ok(),
    })
}

fn collect(artifact: &Path) -> anyhow::Result<Vec<Certified>> {
    if artifact.is_dir() {
        let mut libraries: Vec<PathBuf> = std::fs::read_dir(artifact)
            .with_context(|| format!("could not read '{}'", artifact.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "codiralib"))
            .collect();
        libraries.sort();
        if libraries.is_empty() {
            bail!("'{}' contains no .codiralib", artifact.display());
        }
        return libraries.iter().map(|l| sidecar(l)).collect();
    }
    if let Some(payload) = crate::ops::exe::read_payload(artifact)? {
        let mut found = Vec::new();
        for (name, bytes) in &payload.libraries {
            if name.ends_with(SIDECAR_SUFFIX) {
                continue;
            }
            let certificate_name = format!("{name}{SIDECAR_SUFFIX}");
            found.push(Certified {
                name: format!("{} (embedded {name})", artifact.display()),
                library: bytes.clone(),
                certificate: payload
                    .libraries
                    .iter()
                    .find(|(n, _)| *n == certificate_name)
                    .map(|(_, b)| b.clone()),
            });
        }
        return Ok(found);
    }
    Ok(vec![sidecar(artifact)?])
}

/// The source directory the envelopes are re-derived from, if one is found.
fn envelope_source(args: &Args) -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = &args.source_dir {
        return Ok(Some(dir.clone()));
    }
    let manifest = if let Some(path) = &args.manifest_path {
        Some(path.clone())
    } else {
        let artifact = std::fs::canonicalize(&args.artifact)
            .with_context(|| format!("could not find '{}'", args.artifact.display()))?;
        let start = if artifact.is_dir() {
            artifact.as_path()
        } else {
            artifact.parent().unwrap_or(&artifact)
        };
        crate::ops::build::find_manifest(start)
    };
    Ok(match manifest {
        Some(manifest) => Some(codira_project::Package::from_file(&manifest)?.source_directory()),
        None => None,
    })
}

/// Every function's envelope as the effect analysis computes it from the
/// sources in `source_dir` now.
fn derive_envelopes(source_dir: &Path) -> anyhow::Result<Vec<DerivedEnvelope>> {
    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    };
    let driver = Driver::with_source_directory(source_dir, config)?;
    let db = driver.db();
    Ok(codira_hir::realtime::envelope_report(db)
        .into_iter()
        .filter_map(|f| {
            let (file, line) = f.location.rsplit_once(':')?;
            Some(DerivedEnvelope {
                name: f.function.full_name(db),
                file: file.to_string(),
                line: line.parse().ok()?,
                values: f
                    .inferred
                    .iter()
                    .map(|d| (d.key.to_string(), d.value.to_string()))
                    .collect(),
            })
        })
        .collect())
}

pub fn certify(args: Args) -> anyhow::Result<ExitStatus> {
    let options = CheckOptions {
        rlimit: args.rlimit,
        source_dir: args.source_dir.clone(),
    };
    // Computed once, on the first certificate that records envelopes.
    let mut derived: Option<Result<Vec<DerivedEnvelope>, String>> = None;
    let mut ok = true;
    for item in collect(&args.artifact)? {
        let Some(text) = &item.certificate else {
            println!(
                "{}: no certificate (build with `codira build --certify`)",
                item.name
            );
            ok = false;
            continue;
        };
        let certificate = match std::str::from_utf8(text)
            .map_err(|e| e.to_string())
            .and_then(Certificate::from_json)
        {
            Ok(certificate) => certificate,
            Err(error) => {
                println!("{}: unreadable certificate: {error}", item.name);
                ok = false;
                continue;
            }
        };
        let mut report: Report = codira_certificate::check(&certificate, &item.library, &options);
        if report.envelopes_recorded > 0 {
            let derived = derived.get_or_insert_with(|| match envelope_source(&args) {
                Ok(Some(dir)) => derive_envelopes(&dir).map_err(|e| {
                    format!(
                        "the source in '{}' could not be analysed: {e:#}",
                        dir.display()
                    )
                }),
                Ok(None) => Err(
                    "no codira.toml in the artifact's directory or a parent, so \
                                 there is no source to re-derive them from; pass --source-dir \
                                 or --manifest-path"
                        .to_string(),
                ),
                Err(e) => Err(format!("{e:#}")),
            });
            match derived {
                Ok(derived) => {
                    codira_certificate::check_envelopes(&certificate, derived, &mut report);
                }
                Err(reason) => report.envelopes_not_rederived = Some(reason.clone()),
            }
        }
        println!(
            "{}
{report}",
            item.name
        );
        ok &= report.passed()
            && !(args.deny_unproven && !report.unproven.is_empty())
            && !(args.require_envelopes && report.envelopes_not_rederived.is_some());
    }
    Ok(ok.into())
}
