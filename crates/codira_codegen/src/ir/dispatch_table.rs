//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use codira_hir::{Body, Expr, ExprId, HirDatabase, InferenceResult};
use inkwell::{
    context::Context,
    module::Module,
    targets::TargetData,
    types::{BasicTypeEnum, FunctionType},
    values::{BasicValueEnum, PointerValue},
};
use rustc_hash::FxHashSet;

use crate::{
    intrinsics::Intrinsic,
    ir::{
        closures::{FnValueTargets, TargetKind},
        function,
        ty::HirTypeCache,
    },
    module_group::ModuleGroup,
    type_info::{HasStaticTypeId, TypeId},
};

/// A dispatch table in IR is a struct that contains pointers to all functions
/// that are called from code. In C terms it looks something like this:
/// ```c
/// struct DispatchTable {
///     int(*foo)(int, int);
///     // .. etc
/// } dispatchTable;
/// ```
///
/// The dispatch table is used to add a patchable indirection when calling a
/// function from IR. The `DispatchTable` is exposed to the Runtime which fills
/// the structure with valid pointers to functions. This basically enables all
/// hot reloading within Codira.
#[derive(Debug, Eq, PartialEq)]
pub struct DispatchTable<'ink> {
    // The LLVM context in which all LLVM types live
    context: &'ink Context,
    // The target for which to create the dispatch table
    target: TargetData,
    // This contains the function that map to the DispatchTable struct fields
    function_to_idx: HashMap<codira_hir::Function, usize>,
    // Prototype to function index
    prototype_to_idx: HashMap<FunctionPrototype, usize>,
    // This contains an ordered list of all the function in the dispatch table
    entries: Vec<DispatchableFunction<'ink>>,
    // Contains a reference to the global value containing the DispatchTable
    table_ref: Option<inkwell::values::GlobalValue<'ink>>,
    //
    table_type: Option<inkwell::types::StructType<'ink>>,
    // The function-value targets of the group, and the index of each one's
    // `codira.fn_cell:` entry by key (see `ir::closures`).
    fn_values: FnValueTargets,
    fn_cell_to_idx: HashMap<String, usize>,
}

/// A `FunctionPrototype` defines a unique signature that can be added to the
/// dispatch table.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FunctionPrototype {
    pub name: String,
    pub arg_types: Vec<Arc<TypeId>>,
    pub ret_type: Arc<TypeId>,
}

/// A `DispatchableFunction` is an entry in the dispatch table that may or may
/// not be pointing to an existing `codira_hir` function.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DispatchableFunction<'ink> {
    pub prototype: FunctionPrototype,
    pub codira_hir: Option<codira_hir::Function>,
    /// The LLVM signature of this slot. Needed to call through the
    /// resolved function pointer (`build_indirect_call`) now that opaque
    /// pointers removed `CallableValue`'s ability to carry a callee's type
    /// alongside its address -- see `gen_function_lookup_by_index`.
    pub ir_type: FunctionType<'ink>,
    /// The LLVM function this slot starts out pointing at, when it is not a
    /// `codira_hir` function: the code of a function-value target, offered
    /// to the runtime under `codira.fn_code:` (see `ir::closures`).
    pub prefill: Option<String>,
}

impl<'ink> DispatchTable<'ink> {
    /// Returns whether the `DispatchTable` contains the specified `function`.
    pub fn contains(&self, function: codira_hir::Function) -> bool {
        self.function_to_idx.contains_key(&function)
    }

    /// Returns a slice containing all the functions in the dispatch table.
    pub fn entries(&self) -> &[DispatchableFunction<'ink>] {
        &self.entries
    }

    /// Generate a function lookup through the `DispatchTable`, equivalent to
    /// something along the lines of: `dispatchTable[i]`, where i is the
    /// index of the function and `dispatchTable` is a struct. Returns the
    /// function's LLVM signature alongside the resolved pointer -- both are
    /// required to call through it with `build_indirect_call` now that
    /// opaque pointers removed `CallableValue`.
    pub fn gen_function_lookup(
        &self,
        db: &dyn HirDatabase,
        table_ref: Option<inkwell::values::GlobalValue<'ink>>,
        builder: &inkwell::builder::Builder<'ink>,
        function: codira_hir::Function,
    ) -> (FunctionType<'ink>, PointerValue<'ink>) {
        let function_name = function.name(db).to_string();

        // Get the index of the function
        let index = *self
            .function_to_idx
            .get(&function)
            .expect("unknown function");

        self.gen_function_lookup_by_index(table_ref, builder, &function_name, index)
    }

    /// Generates a function lookup through the `DispatchTable`, equivalent to
    /// something along the lines of: `dispatchTable[i]`, where i is the
    /// index of the intrinsic and `dispatchTable` is a struct
    pub fn gen_intrinsic_lookup(
        &self,
        table_ref: Option<inkwell::values::GlobalValue<'ink>>,
        builder: &inkwell::builder::Builder<'ink>,
        intrinsic: &impl Intrinsic,
    ) -> (FunctionType<'ink>, PointerValue<'ink>) {
        let prototype = intrinsic.prototype();

        // Get the index of the intrinsic
        let index = *self
            .prototype_to_idx
            .get(&prototype)
            .expect("unknown function");

        self.gen_function_lookup_by_index(table_ref, builder, &prototype.name, index)
    }

    /// Generates a function lookup through the `DispatchTable`, equivalent to
    /// something along the lines of: `dispatchTable[i]`, where i is the
    /// index and `dispatchTable` is a struct
    fn gen_function_lookup_by_index(
        &self,
        table_ref: Option<inkwell::values::GlobalValue<'ink>>,
        builder: &inkwell::builder::Builder<'ink>,
        function_name: &str,
        index: usize,
    ) -> (FunctionType<'ink>, PointerValue<'ink>) {
        // Get the internal table reference
        let table_ref = table_ref.expect("no dispatch table defined");
        let table_type = self.table_type.expect("no dispatch table type defined");

        // Create an expression that finds the associated field in the table and returns
        // this as a pointer access
        let ptr_to_function_ptr = builder
            .build_struct_gep(
                table_type,
                table_ref.as_pointer_value(),
                index as u32,
                &format!("{function_name}_ptr_ptr"),
            )
            .unwrap_or_else(|_| {
                panic!("could not get {function_name} (index: {index}) from dispatch table")
            });

        let ir_type = self.entries[index].ir_type;
        let function_ptr = builder
            .build_load(
                self.context.ptr_type(inkwell::AddressSpace::default()),
                ptr_to_function_ptr,
                &format!("{function_name}_ptr"),
            )
            .expect("failed to build load for dispatch table entry")
            .into_pointer_value();

        (ir_type, function_ptr)
    }

    /// The function-value targets of the module group (see `ir::closures`).
    pub(crate) fn fn_values(&self) -> &FnValueTargets {
        &self.fn_values
    }

    /// Loads the address of the runtime-owned code cell for the
    /// function-value target `key`, from its `codira.fn_cell:` slot.
    pub(crate) fn gen_fn_value_cell_lookup(
        &self,
        table_ref: Option<inkwell::values::GlobalValue<'ink>>,
        builder: &inkwell::builder::Builder<'ink>,
        key: &str,
    ) -> PointerValue<'ink> {
        let index = *self
            .fn_cell_to_idx
            .get(key)
            .expect("every function-value target has a cell slot");
        self.gen_function_lookup_by_index(table_ref, builder, "fn_cell", index)
            .1
    }

    /// Returns the value that represents the dispatch table in IR or `None` if
    /// no table was generated.
    pub fn global_value(&self) -> Option<&inkwell::values::GlobalValue<'ink>> {
        self.table_ref.as_ref()
    }

    /// Returns the IR type of the dispatch table's global value, if it exists.
    pub fn ty(&self) -> Option<inkwell::types::StructType<'ink>> {
        self.table_type
    }
}

/// A struct that can be used to build the dispatch table from HIR.
pub(crate) struct DispatchTableBuilder<'db, 'ink, 't> {
    db: &'db dyn HirDatabase,
    // The LLVM context in which all LLVM types live
    context: &'ink Context,
    // The module in which all values live
    module: &'t Module<'ink>,
    // The target for which to create the dispatch table
    target_data: TargetData,
    // Converts HIR ty's to inkwell types
    hir_types: &'t HirTypeCache<'db, 'ink>,
    // This contains the functions that map to the DispatchTable struct fields
    function_to_idx: HashMap<codira_hir::Function, usize>,
    // Prototype to function index
    prototype_to_idx: HashMap<FunctionPrototype, usize>,
    // These are *all* called functions in the modules
    entries: Vec<TypedDispatchableFunction<'ink>>,
    // Contains a reference to the global value containing the DispatchTable
    table_ref: Option<inkwell::values::GlobalValue<'ink>>,
    // This is the actual DispatchTable type
    table_type: inkwell::types::StructType<'ink>,
    // The group of modules for which the dispatch table is being build
    module_group: &'t ModuleGroup,
    // The set of modules that is referenced
    referenced_modules: FxHashSet<codira_hir::Module>,
    // The function-value targets and their cell slots
    fn_values: FnValueTargets,
    fn_cell_to_idx: HashMap<String, usize>,
    // Functions of other modules this group emits a copy of, which are
    // called directly (see `ir::file_group::code_units`)
    local_copies: FxHashSet<codira_hir::Function>,
}

struct TypedDispatchableFunction<'ink> {
    function: DispatchableFunction<'ink>,
    ir_type: FunctionType<'ink>,
}

impl<'db, 'ink, 't> DispatchTableBuilder<'db, 'ink, 't> {
    /// Creates a new builder that can generate a dispatch function.
    pub fn new(
        context: &'ink Context,
        target_data: TargetData,
        db: &'db dyn HirDatabase,
        module: &'t Module<'ink>,
        intrinsics: &BTreeMap<FunctionPrototype, FunctionType<'ink>>,
        hir_types: &'t HirTypeCache<'db, 'ink>,
        module_group: &'t ModuleGroup,
    ) -> Self {
        let mut table = Self {
            db,
            context,
            module,
            target_data,
            function_to_idx: HashMap::default(),
            prototype_to_idx: HashMap::default(),
            entries: Vec::default(),
            table_ref: None,
            table_type: context.opaque_struct_type("DispatchTable"),
            hir_types,
            module_group,
            referenced_modules: FxHashSet::default(),
            fn_values: FnValueTargets::default(),
            fn_cell_to_idx: HashMap::default(),
            local_copies: FxHashSet::default(),
        };

        if !intrinsics.is_empty() {
            table.ensure_table_ref();

            // Use a `BTreeMap` to guarantee deterministically ordered output
            for (prototype, ir_type) in intrinsics.iter() {
                let index = table.entries.len();
                table.entries.push(TypedDispatchableFunction {
                    function: DispatchableFunction {
                        prototype: prototype.clone(),
                        codira_hir: None,
                        ir_type: *ir_type,
                        prefill: None,
                    },
                    ir_type: *ir_type,
                });

                table.prototype_to_idx.insert(prototype.clone(), index);
            }
        }
        table
    }

    /// Creates the global dispatch table in the module if it does not exist.
    fn ensure_table_ref(&mut self) {
        if self.table_ref.is_none() {
            self.table_ref = Some(
                self.module
                    .add_global(self.table_type, None, "dispatchTable"),
            );
        }
    }

    /// Collects call expression from the given expression and sub expressions.
    fn collect_expr(&mut self, expr_id: ExprId, body: &Arc<Body>, infer: &InferenceResult) {
        let expr = &body[expr_id];

        // If this expression is a call, store it in the dispatch table. A
        // builtin call has no callee definition: its runtime functions are
        // intrinsics, collected by `ir::intrinsics`.
        if infer.builtin_call(expr_id).is_some() {
        } else if let Expr::Call { callee, .. } = expr {
            match infer[*callee].as_callable_def() {
                Some(codira_hir::CallableDef::Function(def)) => {
                    // An `extern "codira-intrinsic"` callee is emitted inline
                    // at the call site and never becomes a call instruction,
                    // so there is no symbol for the runtime to resolve. It
                    // must not reach the dispatch table: it is `IS_EXTERN`,
                    // so it would be recorded as an unresolved dependency,
                    // and loading the assembly would then fail with
                    // "Missing dependencies for functions: [load_u32, ..]"
                    // for symbols that are not supposed to exist.
                    // An `@asm` function is the same: its assembly is emitted at
                    // the call site.
                    // A generic callee is instantiated in this group, and a
                    // local copy is too: both are called directly, so
                    // neither needs a slot.
                    if def.is_intrinsic(self.db)
                        || def.is_inline_asm(self.db)
                        || def.is_generic(self.db)
                        || self.local_copies.contains(&def)
                    {
                        // Still walk the arguments -- they are ordinary
                        // expressions and may contain real calls.
                        expr.walk_child_exprs(|expr_id| self.collect_expr(expr_id, body, infer));
                        return;
                    }
                    if self.module_group.should_runtime_link_fn(self.db, def) {
                        let fn_module = def.module(self.db);
                        if !def.is_extern(self.db) && !self.module_group.contains(fn_module) {
                            self.referenced_modules.insert(fn_module);
                        }
                        self.collect_fn_def(def);
                    }
                }
                Some(codira_hir::CallableDef::Struct(_)) => (),
                // A call through a function value: no callee to link.
                None if infer.is_indirect_call(expr_id) => (),
                None => panic!("expected a callable expression"),
            }
        } else if let Expr::MethodCall { .. } = expr {
            // A method or an associated function of another module's type is
            // linked like a free function: `p.describe()`, `Point.origin()`.
            if let Some(def) = infer.method_resolution(expr_id).map(codira_hir::Function::from) {
                let local = def.is_intrinsic(self.db)
                    || def.is_inline_asm(self.db)
                    || def.is_generic(self.db)
                    || self.local_copies.contains(&def);
                if !local && self.module_group.should_runtime_link_fn(self.db, def) {
                    let fn_module = def.module(self.db);
                    if !def.is_extern(self.db) && !self.module_group.contains(fn_module) {
                        self.referenced_modules.insert(fn_module);
                    }
                    self.collect_fn_def(def);
                }
            }
        }

        // Recurse further
        expr.walk_child_exprs(|expr_id| self.collect_expr(expr_id, body, infer));
    }

    /// Adds the two entries of every function-value target (see
    /// `ir::closures`): the empty `codira.fn_cell:` slot the runtime fills
    /// with the target's cell, and the `codira.fn_code:` slot pre-filled with
    /// its code. A thunk calls its function like any call does, so a
    /// function of another module gets its ordinary entry too.
    pub fn collect_fn_values(&mut self, fn_values: FnValueTargets) {
        if fn_values.targets().is_empty() {
            self.fn_values = fn_values;
            return;
        }
        self.ensure_table_ref();
        for target in fn_values.targets() {
            if let TargetKind::Item(item) = target.kind {
                if self.module_group.should_runtime_link_fn(self.db, item)
                    && !item.is_generic(self.db)
                    && !self.local_copies.contains(&item)
                {
                    let fn_module = item.module(self.db);
                    if !item.is_extern(self.db) && !self.module_group.contains(fn_module) {
                        self.referenced_modules.insert(fn_module);
                    }
                    self.collect_fn_def(item);
                }
            }

            let slot_type = self.context.void_type().fn_type(&[], false);
            let cell_index = self.entries.len();
            self.push_named_entry(
                format!("{}{}", codira_abi::FN_VALUE_CELL_PREFIX, target.key),
                slot_type,
                None,
            );
            self.fn_cell_to_idx.insert(target.key.clone(), cell_index);

            let code_type = self.hir_types.get_fn_value_code_type(&target.fn_ty);
            self.push_named_entry(
                format!("{}{}", codira_abi::FN_VALUE_CODE_PREFIX, target.key),
                code_type,
                Some(target.symbol.clone()),
            );
        }
        self.fn_values = fn_values;
    }

    /// Adds an entry that is not a `codira_hir` function: no arguments, no
    /// result, as far as the runtime's signature check is concerned.
    fn push_named_entry(
        &mut self,
        name: String,
        ir_type: FunctionType<'ink>,
        prefill: Option<String>,
    ) {
        let prototype = FunctionPrototype {
            name,
            arg_types: Vec::new(),
            ret_type: <()>::type_id().clone(),
        };
        let index = self.entries.len();
        self.entries.push(TypedDispatchableFunction {
            function: DispatchableFunction {
                prototype: prototype.clone(),
                codira_hir: None,
                ir_type,
                prefill,
            },
            ir_type,
        });
        self.prototype_to_idx.insert(prototype, index);
    }

    /// Collects function call expression from the given expression.
    #[allow(clippy::map_entry)]
    pub fn collect_fn_def(&mut self, function: codira_hir::Function) {
        self.ensure_table_ref();

        // If the function is not yet contained in the table, add it
        if !self.function_to_idx.contains_key(&function) {
            // An `extern` function is a runtime service or another
            // library's symbol, which the runtime finds by its own name,
            // whatever module declares it: `codira_fs_open` declared in
            // `std/fs/fs.code` is still `codira_fs_open`, not
            // `fs::fs::std::codira_fs_open`. (Only root-module declarations
            // used to work.)
            let name = if function.is_extern(self.db) {
                function.name(self.db).to_string()
            } else {
                function.full_name(self.db)
            };
            let hir_type = function.ty(self.db);
            let sig = hir_type.callable_sig(self.db).unwrap();
            // The runtime populates dispatch table slots from each module's
            // `FunctionDefinition::fn_ptr` reflection metadata, which always
            // points at the `_wrapper` function (see `gen_fn_wrapper`) -- the
            // same C-ABI-compatible entry point used for host-language
            // marshalling. The slot's declared type must match that, not the
            // plain internal signature, or a call through it uses the wrong
            // calling convention (visible for struct-by-value returns: the
            // wrapper GC-boxes them, so the real ABI return is a boxed
            // reference, not the raw by-value struct). `gen_call` boxes
            // struct-by-value arguments and unboxes a struct-by-value return
            // to match.
            let ir_type = self
                .hir_types
                .get_public_function_type(&codira_hir::mono::Instance::of(function));
            let arg_types = sig
                .params()
                .iter()
                .map(|arg| self.hir_types.type_id(arg))
                .collect();
            let ret_type = self.hir_types.ret_type_id(sig.ret());

            let prototype = FunctionPrototype {
                name,
                arg_types,
                ret_type,
            };
            let index = self.entries.len();
            self.entries.push(TypedDispatchableFunction {
                function: DispatchableFunction {
                    prototype: prototype.clone(),
                    codira_hir: Some(function),
                    ir_type,
                    prefill: None,
                },
                ir_type,
            });
            self.prototype_to_idx.insert(prototype, index);
            self.function_to_idx.insert(function, index);
        }
    }

    /// Collect all the call expressions from the specified body with the given
    /// type inference result.
    pub fn collect_body(&mut self, body: &Arc<Body>, infer: &InferenceResult) {
        self.collect_expr(body.body_expr(), body, infer);
        // A loop over an iterator object calls its `next` (see
        // `body::pipelines`), like a call written out would.
        for (_, next) in infer.iterator_nexts() {
            if next.is_generic(self.db) || self.local_copies.contains(&next) {
                continue;
            }
            if self.module_group.should_runtime_link_fn(self.db, next) {
                let fn_module = next.module(self.db);
                if !self.module_group.contains(fn_module) {
                    self.referenced_modules.insert(fn_module);
                }
                self.collect_fn_def(next);
            }
        }
    }

    /// Records the functions of other modules this group emits a copy of,
    /// which must therefore not be called through the table.
    pub fn set_local_copies(&mut self, copies: FxHashSet<codira_hir::Function>) {
        self.local_copies = copies;
    }

    /// Builds the final `DispatchTable` with all *called* functions from within
    /// the module
    ///
    /// # Parameters
    ///
    /// * **functions**: Mapping of *defined* Codira functions to their
    ///   respective IR values.
    ///
    /// Returns the `DispatchTable` and a set of dependencies for the module.
    pub fn build(self) -> (DispatchTable<'ink>, FxHashSet<codira_hir::Module>) {
        // Construct the table body from all the entries in the dispatch table
        let table_body: Vec<BasicTypeEnum<'ink>> = self
            .entries
            .iter()
            .map(|f| {
                // Every dispatch-table slot is a function pointer. Under
                // opaque pointers the pointee signature does not change the
                // LLVM type, but `ir_type` is still the record of what the
                // slot holds and is read elsewhere for call construction.
                let _signature = f.ir_type;
                self.context
                    .ptr_type(inkwell::AddressSpace::default())
                    .into()
            })
            .collect();

        // We can fill in the DispatchTable body, i.e: struct DispatchTable { <this
        // part> };
        self.table_type.set_body(&table_body, false);

        // Create a default initializer for function that are already known
        if let Some(table_ref) = self.table_ref {
            let values: Vec<BasicValueEnum<'ink>> = self
                .entries
                .iter()
                .enumerate()
                // Maps over all HIR functions
                .map(|(i, entry)| {
                    let function_type = table_body[i].into_pointer_type();
                    // Find the associated IR function if it exists
                    match entry.function.codira_hir {
                        // The code of a function-value target, defined in
                        // the file module and declared here.
                        None if entry.function.prefill.is_some() => {
                            let symbol = entry.function.prefill.as_deref().unwrap_or_default();
                            self.module
                                .get_function(symbol)
                                .unwrap_or_else(|| {
                                    self.module.add_function(symbol, entry.ir_type, None)
                                })
                                .as_global_value()
                                .as_pointer_value()
                        }
                        // Case external function: Convert to typed null for the given function
                        None => function_type.const_null(),
                        // Case external function, or function from another module
                        Some(f) => {
                            if f.is_extern(self.db)
                                || !self.module_group.contains(f.module(self.db))
                            {
                                // If the function is externally defined (i.e. it's an `extern`
                                // function or it's defined in another module) don't initialize.
                                function_type.const_null()
                            } else {
                                // Otherwise generate a function prototype
                                function::gen_prototype(
                                    self.db,
                                    self.hir_types,
                                    &codira_hir::mono::Instance::of(f),
                                    self.module,
                                    false,
                                )
                                .as_global_value()
                                .as_pointer_value()
                            }
                        }
                    }
                    .into()
                })
                .collect();
            // Set the initialize for the global value
            table_ref.set_initializer(&self.table_type.const_named_struct(&values));
        }

        let table_type = self.table_ref.map(|_| self.table_type);

        (
            DispatchTable {
                context: self.context,
                target: self.target_data,
                function_to_idx: self.function_to_idx,
                prototype_to_idx: self.prototype_to_idx,
                table_ref: self.table_ref,
                table_type,
                fn_values: self.fn_values,
                fn_cell_to_idx: self.fn_cell_to_idx,
                entries: self
                    .entries
                    .into_iter()
                    .map(|entry| entry.function)
                    .collect(),
            },
            self.referenced_modules,
        )
    }
}
