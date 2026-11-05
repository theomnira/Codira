//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - `codira audit`: what a package brings in besides its own Codira code, and
//!   what in it is trusted rather than checked.
//!
//! The report lists
//!
//! - the package's licence fields (`license`, `license-file`);
//! - its `[native]` C, C++ and assembly sources, each with its size, SHA-256
//!   and the SPDX licence identifier it declares, and the include directories,
//!   defines, flags and system libraries they are built with;
//! - every `extern` declaration by ABI, and among them the unchecked trusted
//!   assertions: an `extern` marked `@noalloc`, `@nopanic`, `@noblock`,
//!   `@realtime` or `@envelope(..)`, and every `@asm` variant (`pure: true` is
//!   an assertion too). The effect analysis takes each at its word;
//! - the standard-library modules the package imports.
//!
//! It fails when something the manifest names is missing (a native source,
//! an include directory, the licence file); with `--strict`, also when the
//! package declares no licence or a native source declares no SPDX
//! identifier. `--json` prints the report as JSON (schema
//! `codira-audit/1`).

use std::path::{Path, PathBuf};

use codira_compiler::{Config, Driver, Target};
use codira_hir::{HasSource, HirDatabase, Package};
use codira_hir_input::SourceRootId;
use codira_syntax::{AstNode, SyntaxKind, SyntaxNode};
use sha2::Digest;

use crate::{ops::build::locate_manifest, ExitStatus};

const SCHEMA: &str = "codira-audit/1";

#[derive(clap::Args)]
pub struct Args {
    /// Path to the manifest of the package to audit
    #[clap(long)]
    manifest_path: Option<PathBuf>,

    /// Print the report as JSON (schema `codira-audit/1`)
    #[clap(long)]
    json: bool,

    /// Also fail when the package declares no licence, or a native source
    /// declares no SPDX licence identifier
    #[clap(long)]
    strict: bool,
}

#[derive(Debug, Clone)]
struct NativeFile {
    path: String,
    language: &'static str,
    bytes: Option<u64>,
    sha256: Option<String>,
    spdx: Option<String>,
}

#[derive(Debug, Clone)]
struct Extern {
    name: String,
    location: String,
    /// `C`, `codira-intrinsic`, ... as written; `codira` when no ABI string
    /// is given.
    abi: String,
    /// The trusted assertions it carries, as written.
    trusted: Vec<String>,
}

pub fn audit(args: Args) -> anyhow::Result<ExitStatus> {
    let manifest_path = locate_manifest(args.manifest_path.as_deref())?;
    let package = codira_project::Package::from_file(&manifest_path)?;
    let root = package.root().to_path_buf();
    let manifest = package.manifest();
    let mut problems: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    // Licence.
    let metadata = manifest.metadata();
    let license_file = metadata.license_file.as_ref().map(|f| {
        let path = root.join(f);
        let hash = file_hash(&path);
        if hash.is_none() {
            problems.push(format!("license-file '{}' does not exist", f.display()));
        }
        (f.display().to_string(), hash)
    });
    if metadata.license.is_none() && metadata.license_file.is_none() {
        let message = "the package declares no licence (`license` or `license-file` in [package])";
        if args.strict {
            problems.push(message.to_string());
        } else {
            notes.push(message.to_string());
        }
    }

    // Native sources.
    let native = manifest.native();
    let mut native_files = Vec::new();
    for source in &native.sources {
        let path = root.join(source);
        let text = std::fs::read(&path).ok();
        if text.is_none() {
            problems.push(format!(
                "native source '{}' does not exist",
                source.display()
            ));
        }
        let spdx = text.as_deref().and_then(spdx_identifier);
        if text.is_some() && spdx.is_none() {
            let message = format!(
                "native source '{}' declares no SPDX-License-Identifier",
                source.display()
            );
            if args.strict {
                problems.push(message);
            } else {
                notes.push(message);
            }
        }
        native_files.push(NativeFile {
            path: source.display().to_string().replace('\\', "/"),
            language: language_of(source),
            bytes: text.as_ref().map(|t| t.len() as u64),
            sha256: text.as_deref().map(sha256_hex),
            spdx,
        });
    }
    for dir in &native.include {
        if !root.join(dir).is_dir() {
            problems.push(format!(
                "native include directory '{}' does not exist",
                dir.display()
            ));
        }
    }

    // Externs and imports, from the front end over the sources (nothing is
    // built: native sources are not compiled for an audit).
    let config = Config {
        target: Target::host_target().expect("unable to determine host target"),
        optimization_lvl: codira_compiler::OptimizationLevel::None,
        mode: codira_compiler::BuildMode::default(),
        out_dir: None,
        emit_ir: false,
    };
    let driver = Driver::with_source_directory(package.source_directory(), config)?;
    let externs = externs(driver.db());
    let std_modules: Vec<String> = driver
        .std_files()
        .map(|f| codira_doc::module_name("std", f))
        .collect();

    let trusted: usize = externs.iter().map(|e| e.trusted.len()).sum();
    if args.json {
        let json = serde_json::json!({
            "schema": SCHEMA,
            "package": { "name": manifest.name(), "version": manifest.version().to_string() },
            "license": metadata.license,
            "license_file": license_file.as_ref().map(|(path, hash)| serde_json::json!({
                "path": path, "sha256": hash,
            })),
            "native": {
                "sources": native_files.iter().map(|f| serde_json::json!({
                    "path": f.path,
                    "language": f.language,
                    "bytes": f.bytes,
                    "sha256": f.sha256,
                    "spdx": f.spdx,
                })).collect::<Vec<_>>(),
                "include": native.include.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "defines": native.defines,
                "flags": native.flags,
                "libs": native.libs,
            },
            "externs": externs.iter().map(|e| serde_json::json!({
                "name": e.name,
                "location": e.location,
                "abi": e.abi,
                "trusted": e.trusted,
            })).collect::<Vec<_>>(),
            "trusted_assertions": trusted,
            "std_modules": std_modules,
            "problems": problems,
            "notes": notes,
        });
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(ExitStatus::from(problems.is_empty()));
    }

    println!("audit of {} v{}", manifest.name(), manifest.version());
    println!("\nlicence");
    println!(
        "    license: {}",
        metadata.license.as_deref().unwrap_or("(not declared)")
    );
    if let Some((path, hash)) = &license_file {
        println!(
            "    license-file: {path} ({})",
            hash.as_deref()
                .map_or("MISSING".to_string(), |h| format!("sha256 {h}"))
        );
    }

    println!("\nnative sources ({})", native_files.len());
    for f in &native_files {
        match (&f.bytes, &f.sha256) {
            (Some(bytes), Some(hash)) => println!(
                "    {} [{}] {bytes} bytes, sha256 {hash}, SPDX: {}",
                f.path,
                f.language,
                f.spdx.as_deref().unwrap_or("none")
            ),
            _ => println!("    {} [{}] MISSING", f.path, f.language),
        }
    }
    for (label, items) in [
        (
            "include",
            native
                .include
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        ),
        ("defines", native.defines.clone()),
        ("flags", native.flags.clone()),
        (
            "system libraries (linked by name, not checked)",
            native.libs.clone(),
        ),
    ] {
        if !items.is_empty() {
            println!("    {label}: {}", items.join(", "));
        }
    }

    println!("\nextern declarations ({})", externs.len());
    for e in &externs {
        println!("    {}: {} (abi \"{}\")", e.location, e.name, e.abi);
        for t in &e.trusted {
            println!("        trusted, unchecked: {t}");
        }
    }

    println!(
        "\nstandard-library modules imported ({})",
        std_modules.len()
    );
    if !std_modules.is_empty() {
        println!("    {}", std_modules.join(", "));
    }

    println!(
        "\nsummary: {} native source(s), {} extern(s), {trusted} unchecked trusted assertion(s), \
         {} std module(s)",
        native_files.len(),
        externs.len(),
        std_modules.len()
    );
    for note in &notes {
        println!("note: {note}");
    }
    for problem in &problems {
        println!("error: {problem}");
    }
    Ok(ExitStatus::from(problems.is_empty()))
}

fn language_of(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("c") => "C",
        Some("cc" | "cpp" | "cxx") => "C++",
        Some("s" | "S" | "asm") => "assembly",
        _ => "unknown",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file_hash(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|b| sha256_hex(&b))
}

/// The `SPDX-License-Identifier:` a source declares in its first 40 lines.
pub(crate) fn spdx_identifier(text: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(text);
    text.lines().take(40).find_map(|line| {
        let at = line.find("SPDX-License-Identifier:")?;
        let id = line[at + "SPDX-License-Identifier:".len()..]
            .trim()
            .trim_end_matches("*/")
            .trim();
        (!id.is_empty()).then(|| id.to_string())
    })
}

/// The ABI string of an extern function: `"C"` in `extern "C" func` or an
/// `extern "C" { .. }` block, `codira` when none is written.
fn abi_of(node: &SyntaxNode) -> String {
    let string_in = |n: &SyntaxNode| {
        n.children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| t.kind() == SyntaxKind::STRING)
            .map(|t| t.text().trim_matches('"').to_string())
    };
    string_in(node)
        .or_else(|| {
            node.children()
                .find(|c| c.kind() == SyntaxKind::EXTERN)
                .and_then(|c| string_in(&c))
        })
        .or_else(|| {
            node.ancestors()
                .find(|a| a.kind() == SyntaxKind::EXTERN_BLOCK)
                .and_then(|b| string_in(&b))
        })
        .unwrap_or_else(|| "codira".to_string())
}

/// Attributes that are trusted assertions on an extern.
const TRUSTED: &[&str] = &[
    "noalloc", "nopanic", "noblock", "realtime", "envelope", "asm",
];

fn externs(db: &dyn HirDatabase) -> Vec<Extern> {
    let mut out = Vec::new();
    for package in Package::all(db) {
        for module in package.modules(db) {
            for function in module.all_functions(db) {
                if !function.is_extern(db)
                    || db.file_source_root(function.file_id(db)) != SourceRootId(0)
                {
                    continue;
                }
                let src = function.source(db);
                let node = src.value.syntax().clone();
                let line = db
                    .line_index(src.file_id)
                    .line_col(src.value.signature_range().start())
                    .line
                    + 1;
                let trusted = super::lint::attribute_names(&node)
                    .into_iter()
                    .filter(|(name, _)| TRUSTED.contains(&name.as_str()))
                    .map(|(_, text)| text.split_whitespace().collect::<Vec<_>>().join(" "))
                    .collect();
                out.push(Extern {
                    name: function.name(db).to_string(),
                    location: format!("{}:{line}", db.file_relative_path(src.file_id)),
                    abi: abi_of(&node),
                    trusted,
                });
            }
        }
    }
    out.sort_by_key(|e| {
        let (file, line) = e.location.rsplit_once(':').unwrap_or((&e.location, "0"));
        (file.to_string(), line.parse::<u32>().unwrap_or(0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spdx_identifiers() {
        assert_eq!(
            spdx_identifier(b"// SPDX-License-Identifier: MIT\nint x;").as_deref(),
            Some("MIT")
        );
        assert_eq!(
            spdx_identifier(b"/* SPDX-License-Identifier: Apache-2.0 OR MIT */\n").as_deref(),
            Some("Apache-2.0 OR MIT")
        );
        assert_eq!(spdx_identifier(b"int main() {}\n"), None);
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
