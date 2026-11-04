//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `codira envelope`: every function's semantic envelope (Codira Semantic
//!   Envelope, version 0) as the effect analysis infers it, beside the
//!   `@envelope` it declares, as text or as JSON (`--json`). Experimental
//!   (Tier B); see `lab/rfcs/0001-semantic-envelope.md`.
//! - The package envelope (version 1): the manifest's `[envelope]` table,
//!   which every `public` function must fit. [`package_envelope_errors`] is
//!   the check `codira check` and `codira build` run.

use std::path::{Path, PathBuf};

use codira_compiler::{Config, Driver, Target};
use codira_hir::{
    realtime::{Envelope, FunctionEnvelope, PackageViolation},
    HirDatabase,
};
use codira_project::Manifest;

use crate::{ops::build::locate_manifest, ExitStatus};

/// The identifier of the JSON layout, bumped on any incompatible change.
/// Envelope version 1 only added the optional `package_envelope` member,
/// which a reader of `codira-envelope/0` can ignore, so it is unchanged.
pub const SCHEMA: &str = "codira-envelope/0";

/// The manifest's `[envelope]` table checked against `db`: what is wrong
/// with the table itself, and every public function that does not fit it.
/// `None` when the manifest has no table.
pub(crate) fn package_envelope(
    manifest: &Manifest,
    db: &dyn HirDatabase,
) -> Option<(Envelope, Vec<PackageViolation>)> {
    let table = manifest.envelope()?;
    let envelope =
        Envelope::from_pairs(table.entries.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let violations = codira_hir::realtime::package_envelope_violations(db, &envelope);
    Some((envelope, violations))
}

/// [`package_envelope`] as error messages, one per problem; empty when the
/// manifest has no `[envelope]` or every public function fits it.
pub(crate) fn package_envelope_errors(manifest: &Manifest, db: &dyn HirDatabase) -> Vec<String> {
    let Some((envelope, violations)) = package_envelope(manifest, db) else {
        return Vec::new();
    };
    envelope
        .errors
        .iter()
        .map(|e| format!("codira.toml: {e}"))
        .chain(violations.into_iter().map(|v| v.message))
        .collect()
}

/// Checks the package envelope of the package at `manifest_path` on its own
/// front end, before anything is compiled, and prints each problem. Returns
/// whether the package fits (trivially, when it has no `[envelope]`).
///
/// `codira build` runs this ahead of compiling so that a package that
/// leaves its envelope produces no library. The front end used is a
/// checking one over the package's `src/` (no native sources are built and
/// nothing is written), so it costs one extra analysis, and only for a
/// package that declares an envelope.
pub(crate) fn check_package_envelope_before_build(manifest_path: &Path) -> anyhow::Result<bool> {
    let package = codira_project::Package::from_file(manifest_path)?;
    if package.manifest().envelope().is_none() {
        return Ok(true);
    }
    let driver = Driver::with_source_directory(package.source_directory(), check_config())?;
    let errors = package_envelope_errors(package.manifest(), driver.db());
    for error in &errors {
        eprintln!("error: {error}");
    }
    if !errors.is_empty() {
        eprintln!(
            "error: {} public function(s) or entries do not fit the package envelope; nothing \
             was built",
            errors.len()
        );
    }
    Ok(errors.is_empty())
}

fn check_config() -> Config {
    Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    }
}

#[derive(clap::Args)]
pub struct Args {
    /// Print JSON (schema `codira-envelope/0`) instead of text
    #[clap(long)]
    json: bool,

    /// Exit with an error when a declared `@envelope`, or the package's
    /// `[envelope]` in codira.toml, is not met
    #[clap(long)]
    check: bool,

    /// Path to the manifest of the project
    #[clap(long, conflicts_with = "path")]
    manifest_path: Option<PathBuf>,

    /// Directory of Codira sources to report on instead of a package
    #[clap(long)]
    path: Option<PathBuf>,
}

pub fn envelope(args: Args) -> anyhow::Result<ExitStatus> {
    let config = check_config();
    let (driver, manifest) = if let Some(path) = &args.path {
        (Driver::with_source_directory(path, config)?, None)
    } else {
        let manifest = locate_manifest(args.manifest_path.as_deref())?;
        let (package, driver) = Driver::with_package_path(&manifest, config)?;
        (driver, Some(package.manifest().clone()))
    };
    let report = codira_hir::realtime::envelope_report(driver.db());
    let package = manifest
        .as_ref()
        .and_then(|m| package_envelope(m, driver.db()));
    if args.json {
        let mut json = to_json(&report);
        if let Some((envelope, violations)) = &package {
            json["package_envelope"] = package_json(envelope, violations);
        }
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        print_text(&report);
        if let Some((envelope, violations)) = &package {
            print_package(envelope, violations);
        }
    }
    let violated = report.iter().any(|f| !f.violated.is_empty())
        || package
            .as_ref()
            .is_some_and(|(e, v)| !e.errors.is_empty() || !v.is_empty());
    Ok(ExitStatus::from(!(args.check && violated)))
}

fn package_json(envelope: &Envelope, violations: &[PackageViolation]) -> serde_json::Value {
    let declared: serde_json::Map<String, serde_json::Value> = envelope
        .clauses
        .iter()
        .map(|c| (c.key.to_string(), serde_json::Value::from(c.value.clone())))
        .collect();
    let violations: Vec<serde_json::Value> = violations
        .iter()
        .map(|v| {
            serde_json::json!({
                "name": v.name,
                "location": v.location,
                "violated": v.keys,
                "message": v.message,
            })
        })
        .collect();
    serde_json::json!({
        "declared": declared,
        "errors": envelope.errors,
        "covers": "every public function, through everything it calls",
        "violations": violations,
    })
}

fn print_package(envelope: &Envelope, violations: &[PackageViolation]) {
    let declared: Vec<String> = envelope
        .clauses
        .iter()
        .map(|c| format!("{} = {}", c.key, c.value))
        .collect();
    let status = if !envelope.errors.is_empty() {
        "INVALID"
    } else if violations.is_empty() {
        "met by every public function"
    } else {
        "NOT MET"
    };
    println!(
        "package envelope (codira.toml): {} ({status})",
        declared.join(", ")
    );
    for error in &envelope.errors {
        println!("    {error}");
    }
    for violation in violations {
        println!("    {}", violation.message);
    }
}

/// The report as JSON.
pub fn to_json(report: &[FunctionEnvelope]) -> serde_json::Value {
    let functions: Vec<serde_json::Value> = report
        .iter()
        .map(|f| {
            let inferred: serde_json::Map<String, serde_json::Value> = f
                .inferred
                .iter()
                .map(|d| {
                    (
                        d.key.to_string(),
                        serde_json::json!({ "value": d.value, "because": d.because }),
                    )
                })
                .collect();
            let declared = f.declared.as_ref().map(|clauses| {
                clauses
                    .iter()
                    .map(|c| (c.key.to_string(), serde_json::Value::from(c.value.clone())))
                    .collect::<serde_json::Map<_, _>>()
            });
            serde_json::json!({
                "name": f.name,
                "location": f.location,
                "extern": f.is_extern,
                // On an `extern` the declaration is trusted, not inferred.
                "basis": if f.is_extern { "trusted-declaration" } else { "static-analysis" },
                "declared": declared,
                "violated": f.violated,
                "inferred": inferred,
            })
        })
        .collect();
    serde_json::json!({
        "schema": SCHEMA,
        "guarantee": "source-level effect analysis; not a statement about generated machine code, \
                      and not enforced at run time",
        "functions": functions,
    })
}

fn print_text(report: &[FunctionEnvelope]) {
    for f in report {
        let values: Vec<String> = f
            .inferred
            .iter()
            .map(|d| format!("{}: {}", d.key, d.value))
            .collect();
        println!("{}: {}", f.location, f.name);
        println!("    inferred: {}", values.join(", "));
        if let Some(clauses) = &f.declared {
            let declared: Vec<String> = clauses
                .iter()
                .map(|c| format!("{}: {}", c.key, c.value))
                .collect();
            let status = if f.is_extern {
                "trusted"
            } else if f.violated.is_empty() {
                "met"
            } else {
                "NOT MET"
            };
            println!("    declared: {} ({status})", declared.join(", "));
        }
        for d in f.inferred.iter().filter(|d| f.violated.contains(&d.key)) {
            if let Some(because) = &d.because {
                println!("    {} is {}: {because}", d.key, d.value);
            }
        }
    }
    let declared = report.iter().filter(|f| f.declared.is_some()).count();
    let violated = report.iter().filter(|f| !f.violated.is_empty()).count();
    println!(
        "{} function(s), {declared} with a declared envelope, {violated} not met",
        report.len()
    );
}
