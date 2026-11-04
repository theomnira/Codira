//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Writes the proof certificate of a linked library (`codira build
//!   --certify`): every function's source and code hashes, its verified
//!   obligations as SMT-LIB2 queries, and its inferred semantic envelope
//!   (`codira_hir::realtime::inferred_values`). The format and the checker are in
//!   `codira_certificate`.

use std::path::Path;

use anyhow::Context as _;
use codira_certificate::{
    sha256_hex, ArtifactRecord, Certificate, CodeRecord, FunctionRecord, ObligationRecord, Query,
    SourceRecord,
};
use codira_codegen::{CodeGenDatabase, ModuleGroupId};
use codira_hir::{
    verify::{ObligationKind, Verdict},
    HasSource,
};
use codira_syntax::{AstNode, TextSize};

/// What an obligation is, in the words of the report.
fn kind_text(kind: &ObligationKind) -> String {
    match kind {
        ObligationKind::Precondition { callee } => format!("precondition of `{callee}`"),
        ObligationKind::RefinedArgument { callee, param } => {
            format!("refinement of argument `{param}` of `{callee}`")
        }
        ObligationKind::Postcondition => "postcondition".to_string(),
        ObligationKind::RefinedReturn => "refinement of the return value".to_string(),
        ObligationKind::InvariantOnEntry => "loop invariant on entry".to_string(),
        ObligationKind::InvariantPreserved => "loop invariant preserved".to_string(),
        ObligationKind::MeasureNonNegative => "termination measure non-negative".to_string(),
        ObligationKind::MeasureDecreases => "termination measure decreases".to_string(),
        ObligationKind::DivisionByZero => "non-zero divisor".to_string(),
        ObligationKind::DivisionOverflow => "no signed division overflow".to_string(),
        ObligationKind::UncheckableClause => "contract clause".to_string(),
        ObligationKind::ChoosePredicate => "`choose` predicate".to_string(),
        ObligationKind::UnverifiedClosure => "closure body (not verified)".to_string(),
    }
}

/// Builds the certificate of `module_group`, whose linked library is
/// `library`.
pub(crate) fn certificate(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
    library: &Path,
) -> anyhow::Result<Certificate> {
    let bytes = std::fs::read(library)
        .with_context(|| format!("could not read '{}' to certify it", library.display()))?;
    let symbols = codira_certificate::locate_functions(&bytes).unwrap_or_default();

    let partition = db.module_partition();
    let mut functions: Vec<codira_hir::Function> = partition[module_group]
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|f| !f.is_extern(db))
        .collect();
    functions.sort_by_cached_key(|f| (f.full_name(db), f.file_id(db)));

    let mut records = Vec::with_capacity(functions.len());
    for &function in &functions {
        let name = function.name(db).to_string();
        let shared_name = functions
            .iter()
            .filter(|f| f.name(db).to_string() == name)
            .count()
            > 1;

        let (code, code_note) = if function.is_generic(db) {
            (
                None,
                Some(
                    "generic: it has no code of its own; each instantiation is compiled where it                      is used (named like `f[i64]`) and is not given a code range"
                        .to_string(),
                ),
            )
        } else if shared_name {
            (
                None,
                Some(format!(
                    "several functions of this library are named `{name}`, so the symbol does not \
                     say which code is this one's"
                )),
            )
        } else {
            match codira_certificate::lookup(&symbols, &name) {
                Some([range]) => match CodeRecord::of(&bytes, *range) {
                    Some(record) => (Some(record), None),
                    None => (None, Some("its symbol lies outside the file".to_string())),
                },
                Some(_) => (
                    None,
                    Some(format!("the library has several symbols named `{name}`")),
                ),
                None => (
                    None,
                    Some(
                        "the library has no symbol for it: the optimizer inlined it into its \
                         callers or removed it, or the target keeps no symbol table"
                            .to_string(),
                    ),
                ),
            }
        };

        let src = function.source(db);
        let file_text = db.file_text(src.file_id);
        let range = src.value.syntax().text_range();
        let (start, end) = (usize::from(range.start()), usize::from(range.end()));
        let line_index = db.line_index(src.file_id);
        let line_of = |offset: TextSize| line_index.line_col(offset).line + 1;
        let source = SourceRecord {
            file: db.file_relative_path(src.file_id).to_string(),
            line: line_of(src.value.signature_range().start()),
            start: start as u64,
            end: end as u64,
            sha256: sha256_hex(&file_text.as_bytes()[start..end]),
        };

        let obligations = codira_hir::verify::certify_function(db, function)
            .into_iter()
            .map(|certified| {
                let obligation = certified.obligation;
                let proven = matches!(obligation.verdict, Verdict::Proven);
                let unproven_because = match &obligation.verdict {
                    Verdict::Proven => None,
                    Verdict::Refuted(cex) => Some(format!("counterexample: {}", cex.call(&name))),
                    Verdict::Unknown(cause) => Some(cause.to_string()),
                };
                let last = certified.queries.len().saturating_sub(1);
                let queries = certified
                    .queries
                    .into_iter()
                    .enumerate()
                    .map(|(idx, smtlib2)| Query {
                        smtlib2,
                        // Every query before the last was `unsat` (the
                        // verifier stops at the first that is not); the last
                        // carries the obligation's verdict.
                        expected: match (&obligation.verdict, idx == last) {
                            (Verdict::Refuted(_), true) => "sat",
                            (Verdict::Unknown(_), true) => "unknown",
                            _ => "unsat",
                        }
                        .to_string(),
                    })
                    .collect();
                ObligationRecord {
                    kind: kind_text(&obligation.kind),
                    line: db
                        .line_index(obligation.site.file_id)
                        .line_col(obligation.site.value.range().start())
                        .line
                        + 1,
                    clause: obligation.clause,
                    proven,
                    unproven_because,
                    runtime_checked: obligation.runtime_checked,
                    queries,
                }
            })
            .collect();

        let envelope = codira_hir::realtime::inferred_values(db, function)
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        records.push(FunctionRecord {
            name: function.full_name(db),
            symbol: name,
            source,
            code,
            code_note,
            obligations,
            envelope: Some(envelope),
        });
    }

    Ok(Certificate {
        format: codira_certificate::FORMAT.to_string(),
        compiler: format!("codira {}", env!("CARGO_PKG_VERSION")),
        solver: codira_smt::z3_version(),
        rlimit: codira_smt::DEFAULT_RLIMIT,
        artifact: ArtifactRecord {
            file: library
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: bytes.len() as u64,
            sha256: sha256_hex(&bytes),
        },
        translation_validation: codira_certificate::TRANSLATION_VALIDATION_NOT_RUN.to_string(),
        envelope_basis: Some(codira_certificate::ENVELOPE_BASIS.to_string()),
        functions: records,
    })
}

/// Writes `library`'s certificate next to it.
pub(crate) fn write_certificate(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
    library: &Path,
) -> anyhow::Result<()> {
    let certificate = certificate(db, module_group, library)?;
    let path = codira_certificate::sidecar_path(library);
    std::fs::write(&path, certificate.to_json())
        .with_context(|| format!("could not write '{}'", path.display()))
}
