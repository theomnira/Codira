//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `codira envelope`: every function's semantic envelope (Codira Semantic
//!   Envelope, version 0) as the effect analysis infers it, beside the
//!   `@envelope` it declares, as text or as JSON (`--json`). Experimental
//!   (Tier B); see `lab/rfcs/0001-semantic-envelope.md`.

use std::path::PathBuf;

use codira_compiler::{Config, Driver, Target};
use codira_hir::realtime::FunctionEnvelope;

use crate::{ops::build::locate_manifest, ExitStatus};

/// The identifier of the JSON layout, bumped on any incompatible change.
pub const SCHEMA: &str = "codira-envelope/0";

#[derive(clap::Args)]
pub struct Args {
    /// Print JSON (schema `codira-envelope/0`) instead of text
    #[clap(long)]
    json: bool,

    /// Exit with an error when a declared `@envelope` is not met
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
    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        out_dir: None,
        emit_ir: false,
        mode: codira_compiler::BuildMode::default(),
    };
    let driver = if let Some(path) = &args.path {
        Driver::with_source_directory(path, config)?
    } else {
        let manifest = locate_manifest(args.manifest_path.as_deref())?;
        Driver::with_package_path(&manifest, config)?.1
    };
    let report = codira_hir::realtime::envelope_report(driver.db());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&to_json(&report))?);
    } else {
        print_text(&report);
    }
    let violated = report.iter().any(|f| !f.violated.is_empty());
    Ok(ExitStatus::from(!(args.check && violated)))
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
