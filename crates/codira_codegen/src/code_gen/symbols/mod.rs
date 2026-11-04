//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{collections::HashSet, convert::TryFrom, ffi::CString};

use codira_abi as abi;
use codira_hir::{mono, HirDatabase, TyKind};
use inkwell::{attributes::Attribute, module::Linkage, types::AnyType};
use ir_type_builder::TypeIdBuilder;
use itertools::Itertools;

use crate::{
    ir::{
        closures::SyntheticStruct,
        dispatch_table::{DispatchTable, DispatchableFunction},
        ty::{guid_from_struct, HirTypeCache},
        type_table::TypeTable,
        types as ir,
    },
    value::{
        AsValue, CanInternalize, Global, IrValueContext, IterAsIrValue, SizedValueType, Value,
    },
};

mod ir_type_builder;

/// Construct a `CodiraFunctionPrototype` struct for the specified HIR function.
fn gen_prototype_from_function<'ink>(
    db: &dyn HirDatabase,
    context: &IrValueContext<'ink, '_, '_>,
    function: codira_hir::Function,
    hir_types: &HirTypeCache<'_, 'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::FunctionPrototype<'ink> {
    let name = function.full_name(db);

    // Internalize the name of the function prototype
    let name_str = CString::new(name.clone())
        .expect("function prototype name is not a valid CString")
        .intern(format!("fn_sig::<{name}>::name"), context);

    // Get the `ir::TypeInfo` pointer for the return type of the function
    let fn_sig = function.ty(db).callable_sig(db).unwrap();
    let return_type = ir_type_builder.construct_from_type_id(&hir_types.ret_type_id(fn_sig.ret()));

    // Construct an array of pointers to `ir::TypeInfo`s for the arguments of the
    // prototype
    let arg_types = fn_sig
        .params()
        .iter()
        .map(|ty| ir_type_builder.construct_from_type_id(&hir_types.type_id(ty)))
        .into_const_private_pointer_or_null(format!("fn_sig::<{name}>::arg_types"), context);

    ir::FunctionPrototype {
        name: name_str.as_value(context),
        signature: ir::FunctionSignature {
            arg_types,
            return_type,
            num_arg_types: fn_sig.params().len() as u16,
        },
    }
}

/// Construct a `CodiraFunctionPrototype` struct for the specified dispatch
/// table function.
fn gen_prototype_from_dispatch_entry<'ink>(
    context: &IrValueContext<'ink, '_, '_>,
    function: &DispatchableFunction<'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::FunctionPrototype<'ink> {
    // Internalize the name of the function prototype
    let name_str = CString::new(function.prototype.name.clone())
        .expect("function prototype name is not a valid CString")
        .intern(
            format!("fn_sig::<{}>::name", function.prototype.name),
            context,
        );

    // Get the `ir::TypeInfo` pointer for the return type of the function
    let return_type = ir_type_builder.construct_from_type_id(&function.prototype.ret_type);

    // Construct an array of pointers to `ir::TypeInfo`s for the arguments of the
    // prototype
    let arg_types = function
        .prototype
        .arg_types
        .iter()
        .map(|type_info| ir_type_builder.construct_from_type_id(type_info))
        .into_const_private_pointer_or_null(
            format!("{}_param_types", function.prototype.name),
            context,
        );

    ir::FunctionPrototype {
        name: name_str.as_value(context),
        signature: ir::FunctionSignature {
            arg_types,
            return_type,
            num_arg_types: function.prototype.arg_types.len() as u16,
        },
    }
}

/// The definition of a garbage-collected struct that has no
/// `codira_hir::Struct` behind it: a function type or a closure environment
/// (see `ir::closures`).
fn gen_synthetic_type_definition<'ink>(
    context: &IrValueContext<'ink, '_, '_>,
    def: &SyntheticStruct<'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::TypeDefinition<'ink> {
    let name = &def.name;
    let target_data = context.type_context.target_data;
    let field_names = def
        .fields
        .iter()
        .enumerate()
        .map(|(idx, (field, _))| {
            CString::new(field.as_str())
                .expect("field name is not a valid CString")
                .intern(format!("struct_info::<{name}>::field_names.{idx}"), context)
                .as_value(context)
        })
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_names"), context);
    let field_types = def
        .fields
        .iter()
        .map(|(_, ty)| ir_type_builder.construct_from_type_id(ty))
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_types"), context);
    let field_offsets = (0..def.fields.len())
        .map(|idx| {
            target_data
                .offset_of_element(&def.ir_type, idx as u32)
                .expect("field index in range") as u16
        })
        .into_const_private_pointer_or_null(
            format!("struct_info::<{name}>::field_offsets"),
            context,
        );
    ir::TypeDefinition {
        name: CString::new(name.as_str())
            .expect("typename is not a valid CString")
            .intern(format!("type_info::<{name}>::name"), context)
            .as_value(context),
        size_in_bits: target_data
            .get_bit_size(&def.ir_type)
            .try_into()
            .expect("could not convert size in bits to smaller size"),
        alignment: target_data
            .get_abi_alignment(&def.ir_type)
            .try_into()
            .expect("could not convert alignment to smaller size"),
        data: ir::TypeDefinitionData::Struct(ir::StructDefinition {
            guid: def.guid,
            field_names,
            field_types,
            field_offsets,
            field_previous_names: Value::<*const *const u8>::null(context),
            num_fields: def
                .fields
                .len()
                .try_into()
                .expect("could not convert num_fields to smaller bit size"),
            memory_kind: abi::StructMemoryKind::Gc,
        }),
    }
}

/// Construct a global that holds a reference to all types. e.g.:
/// `CodiraTypeInfo[] definitions = { ... }`
fn get_type_definition_array<'ink>(
    db: &dyn HirDatabase,
    context: &IrValueContext<'ink, '_, '_>,
    types: impl Iterator<Item = codira_hir::Ty>,
    synthetic: &[SyntheticStruct<'ink>],
    hir_types: &HirTypeCache<'_, 'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> Value<'ink, *const ir::TypeDefinition<'ink>> {
    let synthetic_definitions: Vec<ir::TypeDefinition<'ink>> = synthetic
        .iter()
        .map(|def| gen_synthetic_type_definition(context, def, ir_type_builder))
        .collect();
    types
        .sorted_by_cached_key(|type_info| match type_info.interned() {
            TyKind::Struct(s, args) => mono::struct_name(db, *s, args),
            TyKind::Enum(e, _) => e.full_name(db),
            _ => type_info.guid_string(db).unwrap_or_default(),
        })
        .map(|type_info| match type_info.interned() {
            TyKind::Struct(s, args) => {
                let inkwell_type = hir_types.get_struct_type(&type_info);
                let struct_name = mono::struct_name(db, *s, args);
                ir::TypeDefinition {
                    name: CString::new(struct_name.clone())
                        .expect("typename is not a valid CString")
                        .intern(format!("type_info::<{struct_name}>::name"), context)
                        .as_value(context),
                    size_in_bits: context
                        .type_context
                        .target_data
                        .get_bit_size(&inkwell_type)
                        .try_into()
                        .expect("could not convert size in bits to smaller size"),
                    alignment: context
                        .type_context
                        .target_data
                        .get_abi_alignment(&inkwell_type)
                        .try_into()
                        .expect("could not convert alignment to smaller size"),
                    data: ir::TypeDefinitionData::Struct(gen_struct_info(
                        db,
                        &type_info,
                        context,
                        hir_types,
                        ir_type_builder,
                    )),
                }
            }
            TyKind::Tuple(..) | TyKind::Optional(_) | TyKind::Enum(..) | TyKind::Str => {
                gen_structural_definition(&type_info, context, hir_types, ir_type_builder, db)
            }
            _ => unreachable!("unsupported export type"),
        })
        .chain(synthetic_definitions)
        .into_const_private_pointer_or_null("fn.get_info.types", context)
}

/// The definition of a tuple, optional or enum type, exported as a value
/// struct: a tuple's fields are named `0`, `1`, ..; an optional's are
/// `is_some` and `value`; an enum's are `tag` and its payload slots (see
/// `codira_hir::EnumLayout`). Its GUID is the hash of the same name
/// `HirTypeCache::type_id` uses, so references to it resolve to this. A
/// tuple or optional is also *named* by that structural name; an enum is
/// named by its path, as a struct is, which is what hot reloading matches
/// an old and a new layout by.
fn gen_structural_definition<'ink>(
    ty: &codira_hir::Ty,
    context: &IrValueContext<'ink, '_, '_>,
    hir_types: &HirTypeCache<'_, 'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
    db: &dyn HirDatabase,
) -> ir::TypeDefinition<'ink> {
    let guid_name = ty
        .guid_string(db)
        .expect("only types with a structural name are exported");
    let name = match ty.interned() {
        TyKind::Enum(e, _) => e.full_name(db),
        _ => guid_name.clone(),
    };
    let inkwell_type = hir_types
        .get_basic_type(ty)
        .expect("a tuple, optional or enum is a basic type")
        .into_struct_type();
    let (field_names, field_types): (Vec<String>, Vec<codira_hir::Ty>) = match ty.interned() {
        TyKind::Tuple(_, elements) => (
            (0..elements.len()).map(|idx| idx.to_string()).collect(),
            elements.iter().cloned().collect(),
        ),
        TyKind::Optional(payload) => (
            vec!["is_some".to_string(), "value".to_string()],
            vec![TyKind::Bool.intern(), payload.clone()],
        ),
        // The bytes are static, so the address is an integer to the runtime:
        // nothing for the collector to trace.
        TyKind::Str => (
            vec!["data".to_string(), "len".to_string()],
            vec![
                TyKind::Int(codira_hir::IntTy::usize()).intern(),
                TyKind::Int(codira_hir::IntTy::usize()).intern(),
            ],
        ),
        TyKind::Enum(e, _) => {
            let layout = e.layout(db);
            (
                std::iter::once(codira_hir::EnumLayout::TAG.to_string())
                    .chain(layout.slots.iter().map(|slot| slot.name.clone()))
                    .collect(),
                std::iter::once(TyKind::Int(codira_hir::IntTy::u32()).intern())
                    .chain(layout.slots.iter().map(|slot| slot.ty.clone()))
                    .collect(),
            )
        }
        _ => unreachable!("only tuples, optionals and enums are exported this way"),
    };

    let target_data = context.type_context.target_data;
    let field_names_value = field_names
        .iter()
        .enumerate()
        .map(|(idx, field_name)| {
            CString::new(field_name.clone())
                .expect("field name is not a valid CString")
                .intern(format!("struct_info::<{name}>::field_names.{idx}"), context)
                .as_value(context)
        })
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_names"), context);
    let field_types_value = field_types
        .iter()
        .map(|field_ty| ir_type_builder.construct_from_type_id(&hir_types.type_id(field_ty)))
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_types"), context);
    let field_offsets = (0..field_types.len())
        .map(|idx| {
            target_data
                .offset_of_element(&inkwell_type, idx as u32)
                .unwrap() as u16
        })
        .into_const_private_pointer_or_null(
            format!("struct_info::<{name}>::field_offsets"),
            context,
        );

    ir::TypeDefinition {
        name: CString::new(name.clone())
            .expect("typename is not a valid CString")
            .intern(format!("type_info::<{name}>::name"), context)
            .as_value(context),
        size_in_bits: target_data
            .get_bit_size(&inkwell_type)
            .try_into()
            .expect("could not convert size in bits to smaller size"),
        alignment: target_data
            .get_abi_alignment(&inkwell_type)
            .try_into()
            .expect("could not convert alignment to smaller size"),
        data: ir::TypeDefinitionData::Struct(ir::StructDefinition {
            guid: abi::Guid::from_str(&guid_name),
            field_names: field_names_value,
            field_types: field_types_value,
            field_offsets,
            // Structural fields are never renamed.
            field_previous_names: Value::<*const *const u8>::null(context),
            num_fields: field_types
                .len()
                .try_into()
                .expect("could not convert num_fields to smaller bit size"),
            memory_kind: abi::StructMemoryKind::Value,
        }),
    }
}

fn gen_struct_info<'ink>(
    db: &dyn HirDatabase,
    struct_ty: &codira_hir::Ty,
    context: &IrValueContext<'ink, '_, '_>,
    hir_types: &HirTypeCache<'_, 'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::StructDefinition<'ink> {
    let TyKind::Struct(hir_struct, args) = struct_ty.interned() else {
        unreachable!("a struct type");
    };
    let hir_struct = *hir_struct;
    let struct_ir = hir_types.get_struct_type(struct_ty);
    let name = mono::struct_name(db, hir_struct, args);
    let fields = hir_struct.fields(db);
    // Read through the type's arguments: a field `value: T` of `Box[i32]`
    // is an `i32`, and the GC traces it as one.
    let field_tys = mono::struct_field_types(db, struct_ty);

    // Construct an array of field names (or null if there are no fields)
    let field_names = fields
        .iter()
        .enumerate()
        .map(|(idx, field)| {
            CString::new(field.name(db).to_string())
                .expect("field name is not a valid CString")
                .intern(format!("struct_info::<{name}>::field_names.{idx}"), context)
                .as_value(context)
        })
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_names"), context);

    // Construct an array of field types (or null if there are no fields)
    let field_types = field_tys
        .iter()
        .map(|field_ty| {
            let field_type_info = hir_types.type_id(field_ty);
            ir_type_builder.construct_from_type_id(&field_type_info)
        })
        .into_const_private_pointer_or_null(format!("struct_info::<{name}>::field_types"), context);

    // Construct an array of field offsets (or null if there are no fields)
    let field_offsets = fields
        .iter()
        .enumerate()
        .map(|(idx, _)| {
            context
                .type_context
                .target_data
                .offset_of_element(&struct_ir, idx as u32)
                .unwrap() as u16
        })
        .into_const_private_pointer_or_null(
            format!("struct_info::<{name}>::field_offsets"),
            context,
        );

    // Previous names from `@renamed(from: "old")`: a null array when no field
    // was renamed, otherwise one entry per field (null where not renamed).
    let previous_names: Vec<Option<String>> =
        fields.iter().map(|field| field.renamed_from(db)).collect();
    let field_previous_names = if previous_names.iter().all(Option::is_none) {
        Value::<*const *const u8>::null(context)
    } else {
        previous_names
            .iter()
            .enumerate()
            .map(|(idx, previous)| match previous {
                Some(previous) => CString::new(previous.clone())
                    .expect("previous field name is not a valid CString")
                    .intern(
                        format!("struct_info::<{name}>::field_previous_names.{idx}"),
                        context,
                    )
                    .as_value(context),
                None => Value::<*const u8>::null(context),
            })
            .into_const_private_pointer_or_null(
                format!("struct_info::<{name}>::field_previous_names"),
                context,
            )
    };

    ir::StructDefinition {
        guid: guid_from_struct(db, struct_ty),
        field_names,
        field_types,
        field_offsets,
        field_previous_names,
        num_fields: fields
            .len()
            .try_into()
            .expect("could not convert num_fields to smaller bit size"),
        memory_kind: hir_struct.data(db).memory_kind,
    }
}

/// Construct a global that holds a reference to all functions. e.g.:
/// `CodiraFunctionDefinition[] definitions = { ... }`
fn get_function_definition_array<'ink, 'a>(
    db: &dyn HirDatabase,
    context: &IrValueContext<'ink, '_, '_>,
    functions: impl Iterator<Item = &'a codira_hir::Function>,
    hir_types: &HirTypeCache<'_, 'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> Global<'ink, [ir::FunctionDefinition<'ink>]> {
    let module = context.module;
    functions
        .sorted_by_cached_key(|f| f.full_name(db))
        .map(|f| {
            let name = f.name(db).to_string();

            // Get the function from the cloned module and modify the linkage of the
            // function.
            let value = module
                // If a wrapper function exists, use that (required for struct types)
                .get_function(&format!("{name}_wrapper"))
                // Otherwise, use the normal function
                .or_else(|| module.get_function(&name))
                .unwrap();

            // Codira functions are private in the assembly: the runtime reaches
            // them through this table's function *pointers*, never by symbol
            // name, and keeping them private is what lets the optimiser treat
            // the whole module as closed.
            //
            // `@export("C")` is the exception, and the reason it exists
            // (`spec/LANGUAGE_SPEC.md` section 10): such a function must stay
            // externally visible under its own unmangled name so C, Rust,
            // Python (`ctypes`) and Node (`ffi`) callers can find it. It is
            // still listed here, so hot reloading and the Codira-side dispatch
            // keep working exactly as before -- the only difference is that
            // the symbol also survives into the assembly's export table.
            if f.export_abi(db).as_deref() == Some("C") {
                value.set_linkage(Linkage::DLLExport);
            } else {
                value.set_linkage(Linkage::Private);
            }

            // Generate the signature from the function
            let prototype =
                gen_prototype_from_function(db, context, *f, hir_types, ir_type_builder);
            ir::FunctionDefinition {
                prototype,
                fn_ptr: Value::<*const fn()>::with_cast(
                    value.as_global_value().as_pointer_value(),
                    context,
                ),
            }
        })
        .into_value(context)
        .into_const_private_global("fn.get_info.functions", context)
}

/// Generate the type lookup table information. e.g.:
/// ```c
/// CodiraTypeLut typeLut = { ... }
/// ```
fn gen_type_lut<'ink>(
    context: &IrValueContext<'ink, '_, '_>,
    type_table: &TypeTable<'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::TypeLut<'ink> {
    let module = context.module;

    // Get a list of all Guids
    let type_ids = type_table
        .entries()
        .iter()
        .map(|ty| ir_type_builder.construct_from_type_id(ty))
        .into_const_private_pointer("fn.get_info.typeLut.typeIds", context);

    let type_names = type_table
        .entries()
        .iter()
        .map(|ty| {
            CString::new(ty.name.as_str())
                .expect("unable to create CString from typeinfo name")
                .intern(&ty.name, context)
                .as_value(context)
        })
        .into_const_private_pointer("fn.get_info.typeLut.typeNames", context);

    let type_ptrs = TypeTable::find_global(module).map_or_else(
        || Value::null(context),
        |type_table| {
            Value::<*mut *const std::ffi::c_void>::with_cast(
                type_table.as_value(context).value,
                context,
            )
        },
    );

    ir::TypeLut {
        type_ids,
        type_ptrs,
        type_names,
        num_entries: type_table.num_types().try_into().expect("too many types"),
    }
}

/// Generate the dispatch table information. e.g.:
/// ```c
/// CodiraDispatchTable dispatchTable = { ... }
/// ```
fn gen_dispatch_table<'ink>(
    context: &IrValueContext<'ink, '_, '_>,
    dispatch_table: &DispatchTable<'ink>,
    ir_type_builder: &TypeIdBuilder<'ink, '_, '_, '_>,
) -> ir::DispatchTable<'ink> {
    let module = context.module;

    // Generate an internal array that holds all the function prototypes
    let prototypes = dispatch_table
        .entries()
        .iter()
        .map(|entry| gen_prototype_from_dispatch_entry(context, entry, ir_type_builder))
        .into_const_private_pointer("fn.get_info.dispatchTable.signatures", context);

    // Get the pointer to the global table (or nullptr if no global table was
    // defined).
    let fn_ptrs = dispatch_table.global_value().map_or_else(
        || Value::null(context),
        |_g| {
            // TODO: This is a hack, the passed module here is a clone of the module with
            // which the dispatch table was created. Because of this we have to
            // lookup the dispatch table global again. There is however not a
            // `GlobalValue::get_name` method so I just hardcoded the name here.
            Value::<*mut *const fn()>::with_cast(
                module
                    .get_global("dispatchTable")
                    .unwrap()
                    .as_pointer_value(),
                context,
            )
        },
    );

    ir::DispatchTable {
        prototypes,
        fn_ptrs,
        num_entries: dispatch_table.entries().len() as u32,
    }
}

/// Constructs IR that exposes the types and symbols in the specified module. A
/// function called `get_info` is constructed that returns a struct
/// `CodiraAssemblyInfo`. See the `codira_abi` crate for the ABI that `get_info`
/// exposes.
#[allow(clippy::too_many_arguments)]
pub(super) fn gen_reflection_ir<'db, 'ink>(
    db: &'db dyn HirDatabase,
    context: &IrValueContext<'ink, '_, '_>,
    module_name: &str,
    function_definitions: &HashSet<codira_hir::Function>,
    type_definitions: &HashSet<codira_hir::Ty>,
    synthetic_types: &[SyntheticStruct<'ink>],
    dispatch_table: &DispatchTable<'ink>,
    type_table: &TypeTable<'ink>,
    hir_types: &HirTypeCache<'db, 'ink>,
    optimization_level: inkwell::OptimizationLevel,
    dependencies: Vec<String>,
) {
    let ir_type_builder = TypeIdBuilder::new(context);

    let num_functions = function_definitions.len() as u32;
    let functions = get_function_definition_array(
        db,
        context,
        function_definitions.iter(),
        hir_types,
        &ir_type_builder,
    );

    // Get the TypeTable global
    let num_types = (type_definitions.len() + synthetic_types.len()) as u32;
    let types = get_type_definition_array(
        db,
        context,
        type_definitions.iter().cloned(),
        synthetic_types,
        hir_types,
        &ir_type_builder,
    );

    // Construct the module info struct
    let module_info = ir::ModuleInfo {
        path: CString::new(module_name)
            .unwrap()
            .intern("module_info::path", context)
            .as_value(context),
        functions: functions.as_value(context),
        num_functions,
        types,
        num_types,
    };

    // Construct the dispatch table struct
    let dispatch_table = gen_dispatch_table(context, dispatch_table, &ir_type_builder);

    let type_lut = gen_type_lut(context, type_table, &ir_type_builder);

    // Construct the actual `get_info` function
    gen_get_info_fn(
        db,
        context,
        module_info,
        dispatch_table,
        type_lut,
        optimization_level,
        dependencies,
    );
    gen_set_allocator_handle_fn(context);
    gen_set_fuel_counter_fn(context);
    gen_get_version_fn(context);
}

/// Construct the actual `get_info` function.
fn gen_get_info_fn<'ink>(
    db: &dyn HirDatabase,
    context: &IrValueContext<'ink, '_, '_>,
    module_info: ir::ModuleInfo<'ink>,
    dispatch_table: ir::DispatchTable<'ink>,
    type_lut: ir::TypeLut<'ink>,
    _optimization_level: inkwell::OptimizationLevel,
    dependencies: Vec<String>,
) {
    let target = db.target();

    // Construct the return type of the `get_info` method. Depending on the C ABI
    // this is either the `CodiraAssemblyInfo` struct or void. On windows the
    // return argument is passed back to the caller through a pointer to the
    // return type as the first argument. e.g.: On Windows:
    // ```c
    // void get_info(CodiraModuleInfo* result) {...}
    // ```
    // Whereas on other platforms the signature of the `get_info` function is:
    // ```c
    // CodiraModuleInfo get_info() { ... }
    // ```
    let get_symbols_type = if target.options.is_like_windows {
        Value::<'ink, fn(*mut ir::AssemblyInfo<'ink>)>::get_ir_type(context.type_context)
    } else {
        Value::<'ink, fn() -> ir::AssemblyInfo<'ink>>::get_ir_type(context.type_context)
    };

    let get_symbols_fn =
        context
            .module
            .add_function("get_info", get_symbols_type, Some(Linkage::DLLExport));

    if target.options.is_like_windows {
        let type_attribute = context.context.create_type_attribute(
            Attribute::get_named_enum_kind_id("sret"),
            ir::AssemblyInfo::get_ir_type(context.type_context).as_any_type_enum(),
        );

        get_symbols_fn.add_attribute(inkwell::attributes::AttributeLoc::Param(0), type_attribute);
    }

    let builder = context.context.create_builder();
    let body_ir = context.context.append_basic_block(get_symbols_fn, "body");
    builder.position_at_end(body_ir);

    let result_ty = Value::<ir::AssemblyInfo<'ink>>::get_ir_type(context.type_context);

    // Get a pointer to the IR value that will hold the return value. Again this
    // differs depending on the C ABI.
    let result_ptr = if target.options.is_like_windows {
        get_symbols_fn
            .get_nth_param(0)
            .unwrap()
            .into_pointer_value()
    } else {
        builder
            .build_alloca(result_ty, "")
            .expect("failed to build alloca for result struct")
    };

    // Get access to the structs internals
    let symbols_addr = builder
        .build_struct_gep(result_ty, result_ptr, 1, "symbols")
        .expect("could not retrieve `symbols` from result struct");
    let dispatch_table_addr = builder
        .build_struct_gep(result_ty, result_ptr, 3, "dispatch_table")
        .expect("could not retrieve `dispatch_table` from result struct");
    let type_lut_addr = builder
        .build_struct_gep(result_ty, result_ptr, 5, "type_lut")
        .expect("could not retrieve `type_lut` from result struct");
    let dependencies_addr = builder
        .build_struct_gep(result_ty, result_ptr, 7, "dependencies")
        .expect("could not retrieve `dependencies` from result struct");
    let num_dependencies_addr = builder
        .build_struct_gep(result_ty, result_ptr, 9, "num_dependencies")
        .expect("could not retrieve `num_dependencies` from result struct");

    // Assign the struct values one by one.
    builder
        .build_store(symbols_addr, module_info.as_value(context).value)
        .expect("failed to build store");
    builder
        .build_store(dispatch_table_addr, dispatch_table.as_value(context).value)
        .expect("failed to build store");
    builder
        .build_store(type_lut_addr, type_lut.as_value(context).value)
        .expect("failed to build store");
    builder
        .build_store(
            dependencies_addr,
            dependencies
                .iter()
                .enumerate()
                .map(|(idx, name)| {
                    CString::new(name.as_str())
                        .expect("could not convert dependency name to string")
                        .intern(format!("dependency{idx}"), context)
                        .as_value(context)
                })
                .into_const_private_pointer_or_null("dependencies", context)
                .value,
        )
        .expect("failed to build store");
    builder
        .build_store(
            num_dependencies_addr,
            context.context.i32_type().const_int(
                u32::try_from(dependencies.len())
                    .expect("too many dependencies")
                    .into(),
                false,
            ),
        )
        .expect("failed to build store");

    // Construct the return statement of the function.
    if target.options.is_like_windows {
        builder.build_return(None).expect("failed to build return");
    } else {
        let result_value = builder
            .build_load(result_ty, result_ptr, "")
            .expect("failed to build load of result struct");
        builder
            .build_return(Some(&result_value))
            .expect("failed to build return");
    }

    // Note: no per-function optimization pass here (the old legacy
    // `PassManager`/`create_pass_manager` this used to call is gone on
    // LLVM 22 -- see `crate::code_gen::optimize_module`'s doc comment).
    // `get_symbols_fn` lives in `context.module`, which the caller
    // (`AssemblyBuilder::build`) already runs a module-wide optimization
    // pass over right after calling this, so nothing is lost.
}

/// Generates a method `void set_allocator_handle(void*)` that stores the
/// argument into the global `allocatorHandle`. This global is used internally
/// to reference the allocator used by this codiralib.
fn gen_set_allocator_handle_fn(context: &IrValueContext<'_, '_, '_>) {
    let set_allocator_handle_fn = context.module.add_function(
        "set_allocator_handle",
        Value::<fn(*const u8)>::get_ir_type(context.type_context),
        Some(Linkage::DLLExport),
    );

    let builder = context.context.create_builder();
    let body_ir = context
        .context
        .append_basic_block(set_allocator_handle_fn, "body");
    builder.position_at_end(body_ir);

    if let Some(allocator_handle_global) = context.module.get_global("allocatorHandle") {
        builder
            .build_store(
                allocator_handle_global.as_pointer_value(),
                set_allocator_handle_fn.get_nth_param(0).unwrap(),
            )
            .expect("failed to build store");
    }

    builder.build_return(None).expect("failed to build return");
}

/// Generates `void set_fuel_counter(int64_t*)`, which points the module's
/// `fuelCounter` global at the runtime's counter. Only fuel-instrumented
/// modules have that global, and only they export the function.
fn gen_set_fuel_counter_fn(context: &IrValueContext<'_, '_, '_>) {
    let Some(fuel_counter_global) = context.module.get_global("fuelCounter") else {
        return;
    };
    let set_fuel_counter_fn = context.module.add_function(
        abi::SET_FUEL_COUNTER_FN_NAME,
        Value::<fn(*const u8)>::get_ir_type(context.type_context),
        Some(Linkage::DLLExport),
    );

    let builder = context.context.create_builder();
    let body_ir = context
        .context
        .append_basic_block(set_fuel_counter_fn, "body");
    builder.position_at_end(body_ir);
    builder
        .build_store(
            fuel_counter_global.as_pointer_value(),
            set_fuel_counter_fn.get_nth_param(0).unwrap(),
        )
        .expect("failed to build store");
    builder.build_return(None).expect("failed to build return");
}

/// Generates a `get_version` method that returns the current abi version.
/// Specifically, it returns the abi version the function was generated in.
fn gen_get_version_fn(context: &IrValueContext<'_, '_, '_>) {
    let get_version_fn = context.module.add_function(
        abi::GET_VERSION_FN_NAME,
        Value::<fn() -> u32>::get_ir_type(context.type_context),
        Some(Linkage::DLLExport),
    );

    let builder = context.context.create_builder();
    let body_ir = context.context.append_basic_block(get_version_fn, "body");
    builder.position_at_end(body_ir);

    builder
        .build_return(Some(&abi::ABI_VERSION.as_value(context).value))
        .expect("failed to build return");
}
