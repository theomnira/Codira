//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::collections::{BTreeMap, HashSet};

use codira_hir::{
    mono::{self, Instance},
    HasVisibility, ModuleDef,
};
use inkwell::{module::Module, values::FunctionValue};

use super::body::ExternalGlobals;
use crate::{
    code_gen::CodeGenContext,
    ir::{
        body::BodyIrGenerator, file_group::FileGroupIr, function, type_table::TypeTable,
        FunctionMap,
    },
    module_group::ModuleGroup,
    value::Global,
};

/// The IR generated for a single source file.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FileIr<'ink> {
    /// The LLVM module that contains the IR
    pub llvm_module: Module<'ink>,
    /// The `codira_hir::Function`s that constitute the file's API.
    pub function_definitions: HashSet<codira_hir::Function>,
    /// The types defined in this file
    pub type_definitions: HashSet<codira_hir::Ty>,
}

/// Adds every tuple, optional and `str` type inside `ty` (including `ty`
/// itself) to `types`. The unit type `()` is skipped -- it is a primitive to
/// the runtime -- and so is anything whose layout is not known (an error type,
/// an uninstantiated parameter).
fn collect_structural_types(
    db: &dyn codira_hir::HirDatabase,
    ty: &codira_hir::Ty,
    types: &mut HashSet<codira_hir::Ty>,
) {
    use codira_hir::{TyKind, TypeWalk};
    ty.walk(&mut |inner| {
        // A tuple holding a channel or a session endpoint -- the pair
        // `S.open()` returns -- is local to the function: those handles never
        // cross into the runtime, and have no exported layout.
        let holds_handle = || {
            let mut found = false;
            inner.walk(&mut |t| {
                found |= matches!(t.interned(), TyKind::Channel(_) | TyKind::Session(..));
            });
            found
        };
        let structural = match inner.interned() {
            TyKind::Tuple(arity, _) => *arity > 0 && !holds_handle(),
            TyKind::Optional(_) | TyKind::Str => true,
            _ => false,
        };
        if structural && inner.guid_string(db).is_some() {
            types.insert(inner.clone());
        }
    });
}

/// Generates IR for the specified file.
pub(crate) fn gen_file_ir<'ink>(
    code_gen: &CodeGenContext<'_, 'ink>,
    group_ir: &FileGroupIr<'ink>,
    module_group: &ModuleGroup,
) -> FileIr<'ink> {
    let llvm_module = code_gen.context.create_module(&module_group.name);

    let hir_types = &code_gen.hir_types;

    // Generate all exposed function and wrapper function signatures.
    //
    // `BTreeMap`, not `HashMap`, to guarantee deterministically ordered
    // output. `functions` is iterated below to emit bodies, and Rust seeds
    // `HashMap`'s hasher differently per process -- so the same source
    // compiled twice produced the same functions in a different order, and
    // with them the LLVM intrinsic declarations their bodies pull in. That
    // is a reproducible-build defect, and it also makes any snapshot of the
    // emitted IR flap between runs.
    //
    // The code units (`ir::file_group::code_units`) are already in a
    // deterministic order -- the group's own functions by id, then
    // the instances and copies they reach in the order they are first
    // called -- so bodies are emitted in that order.
    let mut functions: FunctionMap<'ink> = FunctionMap::default();
    let mut ordered: Vec<(usize, FunctionValue<'ink>)> = Vec::new();
    let mut type_definitions = HashSet::new();
    let mut wrapper_functions = BTreeMap::new();
    for (index, unit) in group_ir.units.iter().enumerate() {
        let fun = function::gen_prototype(
            code_gen.db,
            hir_types,
            &unit.instance,
            &llvm_module,
            unit.local_copy,
        );
        functions.insert(unit.instance.clone(), fun);
        ordered.push((index, fun));

        // An instance or a copy is internal to this module: it has no
        // host-visible wrapper and is never exported.
        if unit.instance.is_generic() || unit.local_copy {
            continue;
        }
        let f = unit.instance.function();

        let fn_sig = f.ty(code_gen.db).callable_sig(code_gen.db).unwrap();
        // A function taking hidden evidence parameters always gets a
        // wrapper with the host-visible signature: the runtime finds a
        // function through it, and must never call the internal one
        // with too few arguments.
        if (f.visibility(code_gen.db).is_externally_visible() && !fn_sig.marshallable(code_gen.db))
            || crate::ir::body::has_evidence(code_gen.db, f)
        {
            let wrapper_fun =
                function::gen_public_prototype(code_gen.db, &code_gen.hir_types, f, &llvm_module);
            wrapper_functions.insert(f, wrapper_fun);
        }
    }

    for def in module_group
        .iter()
        .flat_map(|module| module.declarations(code_gen.db))
    {
        if let ModuleDef::Enum(e) = def {
            // A generic enum is defined per instantiation, below.
            if e.is_generic(code_gen.db) {
                continue;
            }
            // An enum is exported by the module that declares it, like a
            // struct, together with the tuples and optionals its payloads
            // use.
            type_definitions.insert(e.ty(code_gen.db));
            for slot in e.layout(code_gen.db).slots.iter() {
                collect_structural_types(code_gen.db, &slot.ty, &mut type_definitions);
            }
        }
        if let ModuleDef::Struct(s) = def {
            // A generic struct is defined per instantiation, below.
            if !s.is_generic(code_gen.db) {
                type_definitions.insert(s.ty(code_gen.db));
                for field in s.fields(code_gen.db) {
                    collect_structural_types(
                        code_gen.db,
                        &field.ty(code_gen.db),
                        &mut type_definitions,
                    );
                }
            }
        }
    }
    type_definitions.extend(group_ir.instance_types.iter().cloned());
    for ty in group_ir.instance_types.iter() {
        if ty.as_struct().is_some() {
            for field_ty in mono::struct_field_types(code_gen.db, ty) {
                collect_structural_types(code_gen.db, &field_ty, &mut type_definitions);
            }
        }
        if let Some(layout) = mono::enum_layout(code_gen.db, ty) {
            for slot in layout.slots.iter() {
                collect_structural_types(code_gen.db, &slot.ty, &mut type_definitions);
            }
        }
    }

    // Tuples, optionals and `str` have no declaration to export them from,
    // but the runtime needs their layout as soon as one crosses into it --
    // as a parameter, a return value, a field or an array element. Every one
    // this group's code mentions -- read at each instance, so a generic body
    // contributes its concrete types -- is exported as an anonymous value
    // struct.
    for unit in group_ir.units.iter() {
        let sig = unit.instance.sig(code_gen.db);
        for ty in sig.params().iter().chain(std::iter::once(sig.ret())) {
            collect_structural_types(code_gen.db, ty, &mut type_definitions);
        }
        for ty in unit.infer.all_types() {
            collect_structural_types(code_gen.db, ty, &mut type_definitions);
        }
    }

    let external_globals = {
        let alloc_handle = group_ir
            .allocator_handle_type
            .map(|ty| llvm_module.add_global(ty, None, "allocatorHandle"));
        let dispatch_table = group_ir
            .dispatch_table
            .ty()
            .map(|ty| llvm_module.add_global(ty, None, "dispatchTable"));
        let type_table = if group_ir.type_table.is_empty() {
            None
        } else {
            Some(llvm_module.add_global(group_ir.type_table.ty(), None, TypeTable::NAME))
        };
        let fuel_counter = group_ir
            .fuel_counter_type
            .map(|ty| llvm_module.add_global(ty, None, "fuelCounter"));
        ExternalGlobals {
            alloc_handle,
            fuel_counter,
            dispatch_table,
            type_table: type_table.map(|g| unsafe { Global::from_raw(g) }),
            hardware_safety: code_gen.hardware_safety,
            safety_refusals: code_gen.safety_refusals.clone(),
        }
    };

    // What Eidos runs with, read once for every body below.
    let eidos = code_gen.eidos_run();

    // Generate the function bodies
    for (index, llvm_function) in ordered.iter() {
        let unit = &group_ir.units[*index];
        let mut code_gen = BodyIrGenerator::new(
            code_gen.context,
            &llvm_module,
            code_gen.db,
            (unit.instance.clone(), unit.infer.clone(), *llvm_function),
            &functions,
            &group_ir.dispatch_table,
            &group_ir.type_table,
            external_globals.clone(),
            &code_gen.hir_types,
            module_group,
            eidos.clone(),
        );

        code_gen.gen_fn_body();
    }

    // The code of every function-value target: closure bodies and thunks
    // (see `ir::closures`). Each is generated here, once, rather than where
    // its value is created, so a target exists even when the function that
    // creates it was folded to a constant.
    for target in group_ir.dispatch_table.fn_values().targets() {
        let code_type = code_gen.hir_types.get_fn_value_code_type(&target.fn_ty);
        let llvm_function = llvm_module.add_function(&target.symbol, code_type, None);
        let mut code_gen = BodyIrGenerator::new(
            code_gen.context,
            &llvm_module,
            code_gen.db,
            (
                target.creator.clone(),
                target.creator_infer.clone(),
                llvm_function,
            ),
            &functions,
            &group_ir.dispatch_table,
            &group_ir.type_table,
            external_globals.clone(),
            &code_gen.hir_types,
            module_group,
            None,
        );
        code_gen.gen_fn_value_target(target);
    }

    for (hir_function, llvm_function) in wrapper_functions.iter() {
        let instance = Instance::of(*hir_function);
        let infer = instance.infer(code_gen.db);
        let mut code_gen = BodyIrGenerator::new(
            code_gen.context,
            &llvm_module,
            code_gen.db,
            (instance, infer, *llvm_function),
            &functions,
            &group_ir.dispatch_table,
            &group_ir.type_table,
            external_globals.clone(),
            &code_gen.hir_types,
            module_group,
            eidos.clone(),
        );

        code_gen.gen_fn_wrapper();
    }

    link_native_bitcode(code_gen, &llvm_module);

    // Optimize all generated function bodies in one pass now that they've
    // all been emitted (LLVM's new pass manager operates module-wide, not
    // per-function -- see `crate::code_gen::optimize_module`'s doc comment).
    //
    // The optimizer folds the offsets of nested aggregates into byte offsets
    // (`{ i64, { i1, i64 }, { i64, i64 } }` becomes "8" and "24"), so it
    // must fold them with the target's layout: this module is created
    // without one, and LLVM's default aligns an `i64` to 4 bytes, which
    // placed the second field of a class holding an optional and a tuple
    // at offset 20 while the runtime's layout of the same object says 24.
    // The layout is only lent for the pass, so the IR the module prints and
    // the alignments already built into it do not change.
    let default_layout = code_gen.context.create_module("default_layout");
    llvm_module.set_data_layout(&code_gen.target_machine.get_target_data().get_data_layout());
    crate::code_gen::optimize_module(
        &llvm_module,
        &code_gen.target_machine,
        code_gen.optimization_level,
    );
    llvm_module.set_data_layout(&default_layout.get_data_layout());

    // Filter private methods. Only the group's own non-generic functions
    // can be exported: an instance or a copy belongs to this module alone.
    let function_definitions: HashSet<codira_hir::Function> = group_ir
        .units
        .iter()
        .filter(|unit| !unit.instance.is_generic() && !unit.local_copy)
        .map(|unit| unit.instance.function())
        .filter(|&f| module_group.should_export_fn(code_gen.db, f))
        .collect();

    FileIr {
        llvm_module,
        function_definitions,
        type_definitions,
    }
}

/// Links the package's C and C++ bitcode into `llvm_module` if it calls any
/// of it, before the module is optimized.
///
/// Linking first is what makes a call into C cost nothing: once the C
/// function is in the same module, LLVM inlines it into its Codira caller,
/// folds constants through it and vectorizes across the boundary, exactly as
/// it would within one C file. A module that calls no native function gets
/// none of it. See `crate::native`.
fn link_native_bitcode<'ink>(code_gen: &CodeGenContext<'_, 'ink>, llvm_module: &Module<'ink>) {
    let native = &code_gen.native;
    if native.bitcode.is_empty() {
        return;
    }
    let calls_native = llvm_module.get_functions().any(|function| {
        function.count_basic_blocks() == 0
            && function
                .get_name()
                .to_str()
                .is_ok_and(|name| native.symbols.contains(name))
    });
    if !calls_native {
        return;
    }
    for path in &native.bitcode {
        let native_module = Module::parse_bitcode_from_path(path, code_gen.context)
            .unwrap_or_else(|e| panic!("could not read bitcode '{}': {e}", path.display()));
        llvm_module
            .link_in_module(native_module)
            .unwrap_or_else(|e| panic!("could not link '{}': {e}", path.display()));
    }
}
