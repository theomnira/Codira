//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_hir::HirDatabase;
use inkwell::{
    module::{Linkage, Module},
    values::FunctionValue,
};

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

    // `@export("C")` makes the function reachable from outside the assembly
    // under its own unmangled name (`spec/LANGUAGE_SPEC.md` section 10).
    //
    // Without `DLLExport` the symbol exists in the object file but is absent
    // from the assembly's export table, so nothing outside can find it: a
    // built `.codiralib` exported only `get_info`, `get_version` and
    // `set_allocator_handle`, and `ctypes.CDLL(..).codira_hypot` failed with
    // "function not found" despite the attribute being written and accepted.
    //
    // This is the single mechanism every other language already understands
    // -- C/C++ link or `dlopen`, Rust `libloading`, Python `ctypes`, Node
    // `ffi` -- so exporting correctly here is what makes all of them work,
    // rather than each needing its own bridge.
    let linkage = match func.export_abi(db).as_deref() {
        Some("C") => Some(Linkage::DLLExport),
        // An unrecognised ABI is left with default linkage rather than
        // guessed at. `"C++"` in particular needs mangled-linkage-name
        // metadata that the backend does not emit yet (section 10 says so),
        // and exporting it under its plain Codira name would produce a symbol
        // no C++ caller could ever link against.
        _ => None,
    };

    module.add_function(&name, ir_ty, linkage)
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
