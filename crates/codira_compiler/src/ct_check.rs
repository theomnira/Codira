//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Runs the machine-code constant-time check (`codira build
//!   --check-constant-time`, see `codira_ctcheck`) on a linked library: every
//!   `oblivious` / `@constant_time` function of the module group, located by
//!   its symbol.

use std::path::Path;

use anyhow::Context as _;
use codira_codegen::{CodeGenDatabase, ModuleGroupId};
use codira_ctcheck::{FunctionSpec, Param, ParamClass, Report};
use codira_hir::{IntBitness, Ty, TyKind};

/// How a value of type `ty` is passed, if it is one register's worth (or two
/// for a 128-bit integer).
fn param_class(ty: &Ty) -> Option<ParamClass> {
    match ty.interned() {
        TyKind::Int(int) if int.bitness == IntBitness::X128 => Some(ParamClass::Integer128),
        TyKind::Int(_) | TyKind::Bool => Some(ParamClass::Integer),
        TyKind::Float(_) => Some(ParamClass::Float),
        _ => None,
    }
}

/// The functions of `module_group` to check, and a note for each that
/// cannot be.
fn specs(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
) -> (Vec<FunctionSpec>, Vec<String>) {
    let partition = db.module_partition();
    let mut functions: Vec<codira_hir::Function> = partition[module_group]
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|f| !f.is_extern(db))
        .collect();
    functions.sort_by_cached_key(|f| (f.full_name(db), f.file_id(db)));

    let mut specs = Vec::new();
    let mut notes = Vec::new();
    for &function in &functions {
        let Some((secret_params, secret_result)) =
            codira_hir::verify::constant_time_signature(db, function)
        else {
            continue;
        };
        let full_name = function.full_name(db);
        let symbol = function.name(db).to_string();
        if function.is_generic(db) {
            notes.push(format!(
                "`{full_name}` not checked: it is generic, and its instantiations are not located"
            ));
            continue;
        }
        if functions
            .iter()
            .filter(|f| f.name(db).to_string() == symbol)
            .count()
            > 1
        {
            notes.push(format!(
                "`{full_name}` not checked: several functions of this library are named \
                 `{symbol}`, so the symbol does not say which code is this one's"
            ));
            continue;
        }
        let params: Option<Vec<Param>> = if function.self_param_ty(db).is_some() {
            None
        } else {
            function
                .params(db)
                .iter()
                .zip(
                    secret_params
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(param, secret)| param_class(param.ty()).map(|class| Param { class, secret }))
                .collect()
        };
        // A result that is not returned in registers is written through a
        // hidden pointer argument, which moves every parameter.
        let ret = function.ret_type(db);
        let params =
            params.filter(|_| param_class(&ret).is_some() || ret.is_empty() || ret.is_never());
        specs.push(FunctionSpec {
            symbol,
            name: full_name,
            params,
            secret_result,
        });
    }
    (specs, notes)
}

/// Checks the constant-time functions of `module_group` in `library`.
/// Returns the report and the notes about functions that were not checked.
pub(crate) fn check_library(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
    library: &Path,
) -> anyhow::Result<(Report, Vec<String>)> {
    let bytes = std::fs::read(library).with_context(|| {
        format!(
            "could not read '{}' to check its constant-time functions",
            library.display()
        )
    })?;
    let (specs, mut notes) = specs(db, module_group);
    let report = codira_ctcheck::check_image(&bytes, &specs).map_err(|e| {
        anyhow::anyhow!(
            "cannot check the machine code of '{}' for constant time: {e}",
            library.display()
        )
    })?;
    // One line per distinct note, naming the functions it applies to.
    let mut grouped: Vec<(&str, Vec<&str>)> = Vec::new();
    for function in &report.functions {
        for note in &function.notes {
            match grouped.iter_mut().find(|(text, _)| text == note) {
                Some((_, names)) => names.push(&function.name),
                None => grouped.push((note, vec![&function.name])),
            }
        }
    }
    for (note, names) in grouped {
        let shown: Vec<String> = names.iter().take(4).map(|n| format!("`{n}`")).collect();
        let more = if names.len() > shown.len() {
            format!(" and {} more", names.len() - shown.len())
        } else {
            String::new()
        };
        notes.push(format!("{}{more}: {note}", shown.join(", ")));
    }
    Ok((report, notes))
}

/// The diagnostic for a failed check: each function and each offending
/// instruction.
pub(crate) fn failure_message(report: &Report, library: &Path) -> String {
    let mut message = format!(
        "the machine code of {} constant-time function(s) in '{}' ({}) breaks the \
         machine-code constant-time rules:",
        report
            .functions
            .iter()
            .filter(|f| !f.violations.is_empty())
            .count(),
        library.display(),
        report.arch,
    );
    for function in report.functions.iter().filter(|f| !f.violations.is_empty()) {
        message.push_str(&format!(
            "\n  `{}` (symbol `{}`):",
            function.name, function.symbol
        ));
        for violation in &function.violations {
            message.push_str(&format!("\n    {violation}"));
        }
    }
    message
}
