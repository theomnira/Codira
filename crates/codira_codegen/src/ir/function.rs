//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_hir::{mono::Instance, HirDatabase};
use inkwell::{
    attributes::{Attribute, AttributeLoc},
    module::{Linkage, Module},
    values::FunctionValue,
};

use crate::ir::ty::HirTypeCache;

/// Generates a `FunctionValue` for a `codira_hir::Function`. This function does
/// not generate a body for the `codira_hir::Function`. That task is left to the
/// `gen_body` function. The reason this is split between two functions is that
/// first all signatures are generated and then all bodies. This allows bodies
/// to reference `FunctionValue` wherever they are declared in the file.
///
/// An instance of a generic function, and a function of another module this
/// group emits a copy of (see `ir::file_group::code_units`), is private to
/// the module and named by [`Instance::name`] -- `util::id[i64]` -- which is
/// deterministic and distinct per instance. Everything else keeps its plain
/// name, which the runtime and `@export` rely on.
pub(crate) fn gen_prototype<'db, 'ink>(
    db: &'db dyn HirDatabase,
    types: &HirTypeCache<'db, 'ink>,
    instance: &Instance,
    module: &Module<'ink>,
    local_copy: bool,
) -> FunctionValue<'ink> {
    let func = instance.function();
    let ir_ty = types.get_function_type(instance);
    if instance.is_generic() || local_copy {
        let value = module.add_function(&instance.name(db), ir_ty, Some(Linkage::Private));
        mark_noreturn(db, func, value);
        mark_inline(db, func, value);
        return value;
    }
    let name = func.name(db).to_string();

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

    let value = module.add_function(&name, ir_ty, linkage);
    mark_noreturn(db, func, value);
    mark_inline(db, func, value);
    mark_constant_time(db, func, value);
    value
}

/// Gives `value` LLVM's `alwaysinline` attribute when `func` is marked
/// `@inline`. Only direct calls in the same library can be inlined; a call
/// through the dispatch table (to a public function of another library)
/// stays a call.
fn mark_inline<'ink>(db: &dyn HirDatabase, func: codira_hir::Function, value: FunctionValue<'ink>) {
    if func.is_inline(db) {
        let context = value.get_type().get_context();
        let kind = Attribute::get_named_enum_kind_id("alwaysinline");
        value.add_attribute(
            AttributeLoc::Function,
            context.create_enum_attribute(kind, 0),
        );
    }
}

/// Marks a `@constant_time` or `oblivious` function for the GC passes
/// (`crate::gc_roots`): it gets neither a frame record nor polls, whose
/// branches and indirect calls its constant-time guarantee rules out, and is
/// scanned conservatively instead.
fn mark_constant_time<'ink>(
    db: &dyn HirDatabase,
    func: codira_hir::Function,
    value: FunctionValue<'ink>,
) {
    if codira_hir::verify::constant_time_signature(db, func).is_some() {
        let context = value.get_type().get_context();
        value.add_attribute(
            AttributeLoc::Function,
            context.create_string_attribute("codira-constant-time", ""),
        );
    }
}

/// Whether a call to `func` can never come back: it returns `never` and
/// has no `uses` clause. A `uses` function returning `never` does return
/// when a handler aborts it (aborts unwind by ordinary returns, see
/// `ir::body::effects`), so the caller checks for that after the call and
/// only then reaches `unreachable`; it is not `noreturn`.
pub(crate) fn never_returns(db: &dyn HirDatabase, func: codira_hir::Function) -> bool {
    func.effects(db).is_empty()
        && func
            .ty(db)
            .callable_sig(db)
            .is_some_and(|sig| sig.ret().is_never())
}

/// Gives `value` LLVM's `noreturn` attribute when `func` never returns
/// (see [`never_returns`]). Its LLVM type is then `void` (see
/// `HirTypeCache::get_function_type`).
fn mark_noreturn<'ink>(
    db: &dyn HirDatabase,
    func: codira_hir::Function,
    value: FunctionValue<'ink>,
) {
    if never_returns(db, func) {
        let context = value.get_type().get_context();
        let kind = Attribute::get_named_enum_kind_id("noreturn");
        value.add_attribute(
            AttributeLoc::Function,
            context.create_enum_attribute(kind, 0),
        );
    }
}

/// Generates a `FunctionValue` for a `codira_hir::Function` that is usable from
/// the public API. This function does not generate a body for the
/// `codira_hir::Function`. That task is left to the `gen_body` function. The
/// reason this is split between two functions is that first all signatures are
/// generated and then all bodies. This allows bodies to reference
/// `FunctionValue` wherever they are declared in the file.
/// Whether `f` is called from the host (and from other modules) through a
/// `_wrapper` with the host-visible signature: a public function whose
/// receiver, parameters or result cross boxed (a value struct, a `String`),
/// a function taking hidden effect evidence, or a `main` returning a
/// `Result`, which the host sees returning its `Ok` payload.
pub(crate) fn needs_host_wrapper(
    db: &dyn codira_hir::HirDatabase,
    f: codira_hir::Function,
) -> bool {
    let fn_sig = f.ty(db).callable_sig(db).unwrap();
    let boxed_receiver = f.self_param_ty(db).is_some_and(|ty| {
        ty.is_string()
            || ty
                .as_struct()
                .is_some_and(|s| s.data(db).memory_kind == codira_hir::StructMemoryKind::Value)
    });
    (codira_hir::HasVisibility::visibility(&f, db).is_externally_visible()
        && (!fn_sig.marshallable(db) || boxed_receiver))
        || crate::ir::body::has_evidence(db, f)
        || codira_hir::main_result(db, f).is_some()
}

pub(crate) fn gen_public_prototype<'db, 'ink>(
    db: &'db dyn HirDatabase,
    types: &HirTypeCache<'db, 'ink>,
    func: codira_hir::Function,
    module: &Module<'ink>,
) -> FunctionValue<'ink> {
    let name = format!("{}_wrapper", func.name(db));
    let ir_ty = types.get_public_function_type(&Instance::of(func));
    let value = module.add_function(&name, ir_ty, None);
    mark_noreturn(db, func, value);
    value
}
