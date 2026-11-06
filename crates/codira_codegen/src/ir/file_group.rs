//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{collections::BTreeMap, sync::Arc};

use codira_hir::{
    mono::{self, Instance},
    HirDatabase, HirDisplay, InferenceResult, ModuleDef, Ty, TyKind, TypeWalk,
};
use inkwell::{module::Module, types::PointerType, values::UnnamedAddress, AddressSpace};
use rustc_hash::FxHashSet;

use super::{
    closures::FnValueTargets,
    dispatch_table::{DispatchTable, DispatchTableBuilder},
    intrinsics,
    type_table::{TypeTable, TypeTableBuilder},
};
use crate::{
    code_gen::CodeGenContext,
    module_group::ModuleGroup,
    value::{IrTypeContext, IrValueContext},
};

/// The IR generated for a group of files. It is used to generate IR for all of
/// the group's files and the resulting `Assembly`'s symbols.
#[derive(Debug, PartialEq, Eq)]
pub struct FileGroupIr<'ink> {
    /// The LLVM module that contains the IR
    pub(crate) llvm_module: Module<'ink>,
    /// The dispatch table
    pub(crate) dispatch_table: DispatchTable<'ink>,
    /// The type table
    pub(crate) type_table: TypeTable<'ink>,
    /// The allocator handle, if it exists
    pub(crate) allocator_handle_type: Option<PointerType<'ink>>,
    /// The type of the `fuelCounter` global, if the group is fuel-instrumented
    pub(crate) fuel_counter_type: Option<PointerType<'ink>>,
    /// The modules that contain code that was referenced from this group of
    /// modules
    pub(crate) referenced_modules: FxHashSet<codira_hir::Module>,
    /// The functions this group emits code for. See [`code_units`].
    pub(crate) units: Vec<CodeUnit>,
    /// The instantiated generic struct types the group's code and structs
    /// use, which it defines for the runtime. See [`instance_struct_types`].
    pub(crate) instance_types: Vec<Ty>,
}

/// One function body the group emits: a function of the group, an instance
/// of a generic function, or a copy of another module's function.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CodeUnit {
    pub instance: Instance,
    /// The function's inference result read at `instance`.
    pub infer: Arc<InferenceResult>,
    /// Whether this is a copy of a non-generic function of another module
    /// (see [`code_units`]), emitted privately and called directly.
    pub local_copy: bool,
}

/// The functions a module group emits code for, in a deterministic order:
/// every non-generic function of its modules, then everything they reach
/// that the group must emit itself.
///
/// That is every instance of a generic function the group's code calls --
/// wherever the generic function is declared, the instance is generated in
/// the group that uses it, so generics cross module and assembly boundaries
/// as source, never as code -- and every function of another module that
/// such an instance calls but cannot reach through the dispatch table: a
/// helper the generic function's own module keeps private, and so does not
/// export. A function the group can see is exported by its own module and
/// called through the table as usual.
///
/// A generic function itself is never a unit. It has no code, so the
/// dispatch table, `@export` and the runtime never see it.
pub(crate) fn code_units(db: &dyn HirDatabase, module_group: &ModuleGroup) -> Vec<CodeUnit> {
    // Sorted, so that the group's own functions keep the order they were
    // always emitted in (their ids').
    let mut roots: Vec<codira_hir::Function> = module_group
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|f| !f.is_extern(db) && !f.is_generic(db))
        .collect();
    roots.sort();
    let foreign = |f: codira_hir::Function| !module_group.contains(f.module(db));
    let instances = mono::collect_instances(db, roots, |f| {
        foreign(f)
            && !module_group
                .iter()
                .any(|module| mono::is_callable_from(db, f, module))
    })
    .unwrap_or_else(|overflow| {
        panic!(
            "instantiation of `{}` does not terminate; the HIR validator reports this before \
             code generation",
            overflow.instance.name(db)
        )
    });
    instances
        .into_iter()
        .map(|(instance, infer)| CodeUnit {
            local_copy: !instance.is_generic() && foreign(instance.function()),
            instance,
            infer,
        })
        .collect()
}

/// Every instantiated generic struct type the group needs defined for the
/// runtime, in a deterministic order: those that appear in the types of its
/// code units, in the fields of its non-generic structs, and -- recursively
/// -- in the fields of those.
///
/// The group that uses an instantiation defines it, just as it generates
/// the instances of generic functions it calls. Two groups that both use
/// `Box[i32]` both define it, with the same GUID; the runtime treats those
/// as one type.
pub(crate) fn instance_struct_types(
    db: &dyn HirDatabase,
    module_group: &ModuleGroup,
    units: &[CodeUnit],
) -> Vec<Ty> {
    let mut roots: Vec<Ty> = Vec::new();
    for unit in units {
        let sig = unit.instance.sig(db);
        roots.extend(sig.params().iter().cloned());
        roots.push(sig.ret().clone());
        roots.extend(unit.instance.self_param_ty(db));
        roots.extend(unit.infer.types().cloned());
    }
    for def in module_group
        .iter()
        .flat_map(|module| module.declarations(db))
    {
        if let ModuleDef::Struct(s) = def {
            if !s.is_generic(db) {
                roots.extend(mono::struct_field_types(db, &s.ty(db)));
            }
        }
        if let ModuleDef::Enum(e) = def {
            if !e.is_generic(db) {
                roots.extend(e.layout(db).slots.iter().map(|slot| slot.ty.clone()));
            }
        }
    }

    let mut found: Vec<Ty> = Vec::new();
    let mut seen: FxHashSet<Ty> = FxHashSet::default();
    while let Some(ty) = roots.pop() {
        let mut new = Vec::new();
        ty.walk(&mut |inner| {
            if let TyKind::Struct(_, args) | TyKind::Enum(_, args) = inner.interned() {
                if !args.is_empty()
                    && !mono::contains_type_param(inner)
                    && seen.insert(inner.clone())
                {
                    new.push(inner.clone());
                }
            }
        });
        // The fields (or payload slots) of a newly found instance can name
        // further ones.
        for instance in new {
            match mono::enum_layout(db, &instance) {
                Some(layout) => roots.extend(layout.slots.iter().map(|slot| slot.ty.clone())),
                None => roots.extend(mono::struct_field_types(db, &instance)),
            }
            found.push(instance);
        }
    }
    found.sort_by_cached_key(|ty| ty.display(db).to_string());
    found
}

/// The modules that declare a struct or enum the group's types name: in the
/// types of its code units, in the fields and payload slots of its own
/// types and of the instances it defines (`instance_types`), at any depth.
///
/// The runtime resolves every type the group's type information mentions
/// against the libraries loaded with it, and it loads only a group's
/// dependencies. A type of another module that the group only *names* -- a
/// payload `ParseError` in `enum Problem { Parse(ParseError) }`, with
/// nothing of `ParseError`'s module called -- must still bring that
/// module's library, or loading fails with "Failed to load type with id
/// ...". So these modules are dependencies as much as those the group
/// calls into.
pub(crate) fn type_dependency_modules(
    db: &dyn HirDatabase,
    module_group: &ModuleGroup,
    units: &[CodeUnit],
    instance_types: &[Ty],
) -> FxHashSet<codira_hir::Module> {
    let mut roots: Vec<Ty> = instance_types.to_vec();
    for unit in units {
        let sig = unit.instance.sig(db);
        roots.extend(sig.params().iter().cloned());
        roots.push(sig.ret().clone());
        roots.extend(unit.instance.self_param_ty(db));
        roots.extend(unit.infer.types().cloned());
    }
    for def in module_group
        .iter()
        .flat_map(|module| module.declarations(db))
    {
        match def {
            ModuleDef::Struct(s) if !s.is_generic(db) => roots.push(s.ty(db)),
            ModuleDef::Enum(e) if !e.is_generic(db) => roots.push(e.ty(db)),
            _ => {}
        }
    }

    let mut modules = FxHashSet::default();
    let mut seen: FxHashSet<Ty> = FxHashSet::default();
    while let Some(ty) = roots.pop() {
        let mut adts = Vec::new();
        ty.walk(&mut |inner| {
            if matches!(inner.interned(), TyKind::Struct(..) | TyKind::Enum(..))
                && !mono::contains_type_param(inner)
                && seen.insert(inner.clone())
            {
                adts.push(inner.clone());
            }
        });
        for adt in adts {
            match adt.interned() {
                TyKind::Struct(s, _) => {
                    modules.insert(s.module(db));
                    roots.extend(mono::struct_field_types(db, &adt));
                }
                TyKind::Enum(e, _) => {
                    modules.insert(e.module(db));
                    if let Some(layout) = mono::enum_layout(db, &adt) {
                        roots.extend(layout.slots.iter().map(|slot| slot.ty.clone()));
                    }
                }
                _ => {}
            }
        }
    }
    // The group's own types are its own: a library does not depend on itself.
    modules.retain(|module| !module_group.contains(*module));
    modules
}

/// Generates IR that is shared among the group's files.
pub(crate) fn gen_file_group_ir<'ink>(
    code_gen: &CodeGenContext<'_, 'ink>,
    module_group: &ModuleGroup,
) -> FileGroupIr<'ink> {
    let llvm_module = code_gen.context.create_module("group_name");

    // Use a `BTreeMap` to guarantee deterministically ordered output.
    let mut intrinsics_map = BTreeMap::new();
    let mut needs_alloc = false;

    // Every body the group emits: its own functions (`all_functions`, which
    // covers `extend` block methods too), the instances of generic
    // functions they reach, and copies of private helpers those need.
    let units = code_units(code_gen.db, module_group);
    let instance_types = instance_struct_types(code_gen.db, module_group, &units);

    // Collect all intrinsic functions, wrapper function, and generate struct
    // declarations.
    for unit in &units {
        let f = unit.instance.function();
        intrinsics::collect_fn_body(
            code_gen.context,
            code_gen.target_machine.get_target_data(),
            code_gen.db,
            &mut intrinsics_map,
            &mut needs_alloc,
            &f.body(code_gen.db),
            &unit.infer,
        );
        intrinsics::collect_main_result(
            code_gen.context,
            code_gen.target_machine.get_target_data(),
            code_gen.db,
            &mut intrinsics_map,
            &mut needs_alloc,
            f,
        );

        // Only the group's own non-generic functions get a host-visible
        // wrapper; an instance or a copy is never exported.
        if unit.instance.is_generic() || unit.local_copy {
            continue;
        }
        if crate::ir::function::needs_host_wrapper(code_gen.db, f) {
            intrinsics::collect_wrapper_body(
                code_gen.context,
                code_gen.target_machine.get_target_data(),
                &mut intrinsics_map,
                &mut needs_alloc,
            );
        }
    }

    // Fuel-instrumented code calls back into the runtime when it runs dry.
    let fuel = code_gen.fuel;
    if fuel {
        intrinsics::collect_fuel(
            code_gen.context,
            code_gen.target_machine.get_target_data(),
            &mut intrinsics_map,
        );
    }

    // The closures and named functions the group turns into function values
    // (see `ir::closures`). A thunk may box value-struct arguments for a
    // call through the dispatch table, which allocates.
    let fn_values = FnValueTargets::collect(code_gen.db, module_group, &units, &instance_types);
    if !fn_values.targets().is_empty() {
        intrinsics::collect_wrapper_body(
            code_gen.context,
            code_gen.target_machine.get_target_data(),
            &mut intrinsics_map,
            &mut needs_alloc,
        );
    }

    // Collect all exposed functions' bodies.
    let mut dispatch_table_builder = DispatchTableBuilder::new(
        code_gen.context,
        code_gen.target_machine.get_target_data(),
        code_gen.db,
        &llvm_module,
        &intrinsics_map,
        &code_gen.hir_types,
        module_group,
    );
    dispatch_table_builder.set_local_copies(
        units
            .iter()
            .filter(|unit| unit.local_copy)
            .map(|unit| unit.instance.function())
            .collect(),
    );
    for unit in &units {
        // Find all functions that must be present in the dispatch table
        let body = unit.instance.function().body(code_gen.db);
        dispatch_table_builder.collect_body(&body, &unit.infer);
        // `main`'s `Err` is written with its type's `to_string`.
        if let Some(to_string) = codira_hir::main_result(code_gen.db, unit.instance.function())
            .and_then(|report| report.to_string)
        {
            dispatch_table_builder.collect_callee(to_string);
        }
    }

    dispatch_table_builder.collect_fn_values(fn_values);
    let (dispatch_table, mut referenced_modules) = dispatch_table_builder.build();
    referenced_modules.extend(type_dependency_modules(
        code_gen.db,
        module_group,
        &units,
        &instance_types,
    ));

    let target_data = code_gen.target_machine.get_target_data();
    let type_context = IrTypeContext {
        context: code_gen.context,
        target_data: &target_data,
        struct_types: &code_gen.rust_types,
    };
    let value_context = IrValueContext {
        type_context: &type_context,
        context: code_gen.context,
        module: &llvm_module,
    };
    let mut type_table_builder = TypeTableBuilder::new(
        code_gen.db,
        &value_context,
        intrinsics_map.keys(),
        &dispatch_table,
        &code_gen.hir_types,
        module_group,
    );

    // Collect all used types
    for def in module_group
        .iter()
        .flat_map(|module| module.declarations(code_gen.db))
    {
        match def {
            // A generic struct has no layout of its own; its instantiations
            // are collected below.
            ModuleDef::Struct(s) if s.is_generic(code_gen.db) => (),
            ModuleDef::Struct(s) => {
                type_table_builder.collect_struct(&s.ty(code_gen.db));
            }
            // Like a generic struct, a generic enum is collected per
            // instantiation, below.
            ModuleDef::Enum(e) if e.is_generic(code_gen.db) => (),
            ModuleDef::Enum(e) => {
                type_table_builder.collect_enum(&e.ty(code_gen.db));
            }
            ModuleDef::Function(_)
            | ModuleDef::PrimitiveType(_)
            | ModuleDef::TypeAlias(_)
            | ModuleDef::Effect(_)
            | ModuleDef::Session(_)
            // A trait has no runtime representation: its calls are
            // resolved per instantiation (see `codira_hir::traits`).
            | ModuleDef::Trait(_)
            | ModuleDef::Module(_) => (),
        }
    }
    for ty in &instance_types {
        if ty.as_enum().is_some() {
            type_table_builder.collect_enum(ty);
        } else {
            type_table_builder.collect_struct(ty);
        }
    }

    for unit in &units {
        type_table_builder.collect_fn(&unit.instance, &unit.infer);
    }
    type_table_builder.collect_fn_values(dispatch_table.fn_values());

    let type_table = type_table_builder.build();

    // Create the allocator handle global value
    let allocator_handle_type = if needs_alloc {
        let allocator_handle_type = code_gen.context.ptr_type(AddressSpace::default());
        let global = llvm_module.add_global(allocator_handle_type, None, "allocatorHandle");
        global.set_initializer(&allocator_handle_type.const_null());
        global.set_unnamed_address(UnnamedAddress::Global);
        Some(allocator_handle_type)
    } else {
        None
    };

    // The fuel counter: a pointer the runtime sets through
    // `set_fuel_counter`. Until it does, it points at a private counter
    // holding `i64::MAX`, so an instrumented library loaded by a host that
    // never sets one runs as if fuel were unlimited.
    let fuel_counter_type = if fuel {
        let counter_type = code_gen.context.ptr_type(AddressSpace::default());
        let fallback = llvm_module.add_global(code_gen.context.i64_type(), None, "fuelFallback");
        fallback.set_initializer(
            &code_gen
                .context
                .i64_type()
                .const_int(i64::MAX as u64, false),
        );
        fallback.set_linkage(inkwell::module::Linkage::Private);
        let global = llvm_module.add_global(counter_type, None, "fuelCounter");
        global.set_initializer(&fallback.as_pointer_value());
        Some(counter_type)
    } else {
        None
    };

    FileGroupIr {
        llvm_module,
        dispatch_table,
        type_table,
        allocator_handle_type,
        fuel_counter_type,
        referenced_modules,
        units,
        instance_types,
    }
}
