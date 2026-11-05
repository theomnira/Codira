//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Runs the experimental superoptimiser (`codira build --superoptimize`,
//!   see `codira_superopt::library`) on a linked library: the straight-line
//!   integer tail of each of the module group's functions is replaced by a
//!   cheaper sequence Z3 proves equal, in place. Functions that must run in
//!   constant time (`oblivious`, `@constant_time`) are never touched.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use codira_codegen::{CodeGenDatabase, ModuleGroupId};
use codira_mcode::profile::{Objective, Profile};
use codira_superopt::{Config, ConstantTime};

/// The search bounds used during a build: smaller than the standalone
/// tool's, so that a library with many functions builds in seconds the
/// first time (and instantly afterwards, from the cache).
fn build_config() -> Config {
    Config {
        max_insts: 3,
        objective: Objective::Latency,
        max_terms: 150_000,
        keep: 2,
        max_rounds: 8,
        time_limit_ms: 5_000,
    }
}

/// Whether each function of `module_group` must run in constant time, by
/// symbol. A symbol shared by several functions, and generic functions
/// (whose instances have other symbols), are left out, so they are not
/// rewritten.
fn origins(db: &dyn CodeGenDatabase, module_group: ModuleGroupId) -> HashMap<String, ConstantTime> {
    let partition = db.module_partition();
    let functions: Vec<codira_hir::Function> = partition[module_group]
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|f| !f.is_extern(db) && !f.is_generic(db))
        .collect();
    let mut count: HashMap<String, usize> = HashMap::new();
    for f in &functions {
        *count.entry(f.name(db).to_string()).or_default() += 1;
    }
    functions
        .iter()
        .filter(|f| count[&f.name(db).to_string()] == 1)
        .map(|&f| {
            let constant_time = if codira_hir::verify::constant_time_signature(db, f).is_some() {
                ConstantTime::Required
            } else {
                ConstantTime::NotRequired
            };
            (f.name(db).to_string(), constant_time)
        })
        .collect()
}

/// Superoptimises `library` in place and returns the lines to report.
pub(crate) fn optimize_library(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
    library: &Path,
) -> anyhow::Result<Vec<String>> {
    let target = db.target();
    if target.arch != "x86_64" {
        return Ok(vec![format!(
            "note: superoptimiser: skipped, it rewrites x86-64 code only (target is {})",
            target.arch
        )]);
    }
    let mut profile = Profile::parse(codira_mcode::profile::X86_64_DEFAULT)
        .map_err(|e| anyhow::anyhow!("the shipped x86-64 cost profile does not parse: {e}"))?;
    // Only extensions the deployment target has (the profile's own list is
    // what the machine it was measured on has).
    let features = &target.options.features;
    profile.features.retain(|f| {
        features
            .split(',')
            .any(|t| t.trim_start_matches('+') == f.as_str() || (f == "bmi1" && t == "+bmi"))
    });

    let mut bytes = std::fs::read(library)
        .with_context(|| format!("could not read '{}' to superoptimise it", library.display()))?;
    let cache = library
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".superopt-cache");
    let report = match codira_superopt::library::optimize_elf(
        &mut bytes,
        &origins(db, module_group),
        &profile,
        &build_config(),
        &cache,
    ) {
        Ok(report) => report,
        Err(e) => return Ok(vec![format!("note: superoptimiser: skipped: {e}")]),
    };
    let mut lines = Vec::new();
    for r in &report.rewrites {
        let show = |b: &codira_mcode::Block<codira_mcode::x86::X86_64>| {
            b.insts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        };
        lines.push(format!(
            "superoptimiser: `{}` at {:#x}: {} => {} (latency {:.1} -> {:.1} cycles, {} -> {} bytes, proven by Z3)",
            r.function,
            r.address,
            show(&r.original),
            show(&r.replacement),
            r.cost_before.latency,
            r.cost_after.latency,
            r.original_bytes,
            r.replacement_bytes,
        ));
    }
    for note in &report.notes {
        lines.push(format!("note: superoptimiser: {note}"));
    }
    lines.push(format!(
        "superoptimiser ({}): {} function(s), {} tail(s) searched, {} rewritten; {} constant-time function(s) left alone",
        profile.id,
        report.functions_seen - report.unknown_skipped,
        report.runs_searched,
        report.rewrites.len(),
        report.constant_time_refused,
    ));
    if !report.rewrites.is_empty() {
        std::fs::write(library, &bytes).with_context(|| {
            format!("could not write the superoptimised '{}'", library.display())
        })?;
    }
    Ok(lines)
}
