//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira doc`: static, searchable HTML documentation for a package or for
//!   the standard library (see `codira_doc`), with each function's inferred
//!   envelope from the compiler's effect analysis.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::anyhow;
use codira_compiler::{Config, Driver, Target};
use codira_doc::{Dimension, DocSet, Envelope, SourceInput};
use codira_hir::HasSource;
use codira_hir_input::SourceDatabase;
use codira_syntax::AstNode;

use crate::{
    ops::{build::locate_manifest, fmt::collect_code_files},
    ExitStatus,
};

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the package to document
    #[clap(long, conflicts_with = "std")]
    manifest_path: Option<PathBuf>,

    /// Document the standard library (the one `CODIRA_STD_PATH` names, or
    /// the one this compiler finds) instead of a package
    #[clap(long)]
    std: bool,

    /// Directory to write the documentation to. Defaults to `target/doc`
    /// in the package (or, with `--std`, in the current directory)
    #[clap(long)]
    out: Option<PathBuf>,

    /// Document items that are not `public` too
    #[clap(long)]
    private: bool,

    /// Leave out the inferred envelope badges (skips the effect analysis)
    #[clap(long)]
    no_envelopes: bool,

    /// List every public item that has no doc comment
    #[clap(long)]
    undocumented: bool,
}

pub fn doc(args: Args) -> anyhow::Result<ExitStatus> {
    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        mode: codira_compiler::BuildMode::default(),
        out_dir: None,
        emit_ir: false,
    };
    let (title, namespace, source_dir, default_out) = if args.std {
        let dir = codira_compiler::std_library_dir().map_err(|e| anyhow!(e))?;
        let out = std::env::current_dir()?.join("target").join("doc");
        ("std".to_string(), "std".to_string(), dir, out)
    } else {
        let manifest = locate_manifest(args.manifest_path.as_deref())?;
        let package = codira_project::Package::from_file(&manifest)?;
        (
            package.name().to_string(),
            package.name().to_string(),
            package.source_directory(),
            package.root().join("target").join("doc"),
        )
    };
    let out_dir = args.out.clone().unwrap_or(default_out);

    let files = read_sources(&source_dir)?;
    if files.is_empty() {
        return Err(anyhow!(
            "'{}' has no .code files to document",
            source_dir.display()
        ));
    }

    let envelopes = if args.no_envelopes {
        HashMap::new()
    } else {
        let driver = Driver::with_source_directory(&source_dir, config)?;
        envelopes(&driver)
    };

    let set = DocSet {
        title,
        namespace,
        files,
        envelopes,
        include_private: args.private,
    };
    let summary = codira_doc::generate(&set, &out_dir)
        .map_err(|e| anyhow!("could not write '{}': {e}", out_dir.display()))?;
    if args.undocumented {
        for item in &summary.undocumented {
            println!("undocumented: {item}");
        }
    }
    println!(
        "documented {} module(s), {} item(s) ({} with an inferred envelope, {} public without a doc comment)",
        summary.modules,
        summary.items,
        summary.with_envelope,
        summary.undocumented.len()
    );
    println!("wrote {}", out_dir.join("index.html").display());
    Ok(ExitStatus::Success)
}

/// Every `.code` file under `dir`, by path relative to it.
fn read_sources(dir: &Path) -> anyhow::Result<Vec<SourceInput>> {
    let mut paths = Vec::new();
    collect_code_files(dir, &mut paths)?;
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| anyhow!("could not read '{}': {e}", path.display()))?;
            let relative = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            Ok(SourceInput {
                path: relative,
                text,
            })
        })
        .collect()
}

/// The inferred envelope of every function in the driver's own sources,
/// by source path and offset of the function's syntax node.
fn envelopes(driver: &Driver) -> HashMap<(String, u32), Envelope> {
    let db = driver.db();
    codira_hir::realtime::envelope_report(db)
        .into_iter()
        .map(|f| {
            let src = f.function.source(db);
            let key = (
                db.file_relative_path(src.file_id).to_string(),
                u32::from(src.value.syntax().text_range().start()),
            );
            let envelope = Envelope {
                dimensions: f
                    .inferred
                    .iter()
                    .map(|d| Dimension {
                        key: d.key.to_string(),
                        present: d.because.is_some(),
                        because: d.because.clone(),
                    })
                    .collect(),
                declared: f.declared.as_ref().map(|clauses| {
                    clauses
                        .iter()
                        .map(|c| (c.key.to_string(), c.value.clone()))
                        .collect()
                }),
                violated: f.violated.iter().map(|k| k.to_string()).collect(),
                is_extern: f.is_extern,
            };
            (key, envelope)
        })
        .collect()
}
