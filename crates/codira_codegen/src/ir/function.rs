//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_hir::HirDatabase;
use inkwell::{module::Module, values::FunctionValue};

use crate::ir::ty::HirTypeCache;

/// Generates a `FunctionValue` for a `codira_hir::Function`. This function does
/// not generate a body for the `codira_hir::Function`. That task is left to the
/// `gen_body` function. The reason this is split between two functions is that
/// first all signatures are generated and then all bodies. This allows bodies
/// to reference `FunctionValue` wherever they are declared in the file.
pub(crate) fn gen_prototype<'db, 'ink>(
    db: &'db dyn HirDatabase,
    types: &HirTypeCache<'db, 'ink>,
    func: codira_hir::Function,
    module: &Module<'ink>,
) -> FunctionValue<'ink> {
    let name = func.name(db).to_string();
    let ir_ty = types.get_function_type(func);
    module.add_function(&name, ir_ty, None)
}

/// Generates a `FunctionValue` for a `codira_hir::Function` that is usable from
/// the public API. This function does not generate a body for the
/// `codira_hir::Function`. That task is left to the `gen_body` function. The
/// reason this is split between two functions is that first all signatures are
/// generated and then all bodies. This allows bodies to reference
/// `FunctionValue` wherever they are declared in the file.
pub(crate) fn gen_public_prototype<'db, 'ink>(
    db: &'db dyn HirDatabase,
    types: &HirTypeCache<'db, 'ink>,
    func: codira_hir::Function,
    module: &Module<'ink>,
) -> FunctionValue<'ink> {
    let name = format!("{}_wrapper", func.name(db));
    let ir_ty = types.get_public_function_type(func);
    module.add_function(&name, ir_ty, None)
}
