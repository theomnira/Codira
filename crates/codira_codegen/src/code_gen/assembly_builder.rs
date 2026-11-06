//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use inkwell::{
    module::{Linkage, Module},
    GlobalVisibility,
};
use rustc_hash::FxHashSet;

use crate::{
    assembly::Assembly,
    code_gen::{optimize_module, symbols, CodeGenContext, CodeGenerationError},
    ir::{file::gen_file_ir, file_group::gen_file_group_ir},
    value::{IrTypeContext, IrValueContext},
    ModuleGroupId, ModulePartition,
};

/// A struct that can be used to build an `Assembly<'db, 'ink', ctx>`
pub struct AssemblyBuilder<'db, 'ink, 'ctx, 't> {
    code_gen: &'ctx CodeGenContext<'db, 'ink>,
    module_group_partition: &'t ModulePartition,
    module_group_id: ModuleGroupId,
    assembly_module: Module<'ink>,
}

impl<'db, 'ink, 'ctx, 't> AssemblyBuilder<'db, 'ink, 'ctx, 't> {
    /// Constructs a new `AssemblyBuilder` for the given module group.
    pub(crate) fn new(
        code_gen: &'ctx CodeGenContext<'db, 'ink>,
        module_group_partition: &'t ModulePartition,
        module_group_id: ModuleGroupId,
    ) -> Self {
        // Construct a module for the assembly
        let module_group = &module_group_partition[module_group_id];
        let assembly_module = code_gen.create_module(&module_group.name);

        Self {
            code_gen,
            module_group_partition,
            module_group_id,
            assembly_module,
        }
    }

    /// Constructs an object file.
    pub fn build(self) -> Result<Assembly<'db, 'ink, 'ctx>, anyhow::Error> {
        let module_group = &self.module_group_partition[self.module_group_id];
        let group_ir = gen_file_group_ir(self.code_gen, module_group);
        let file = gen_file_ir(self.code_gen, &group_ir, module_group);

        // Clone the LLVM modules so that we can modify it without modifying the cached
        // value.
        self.assembly_module
            .link_in_module(group_ir.llvm_module.clone())
            .map_err(|e| CodeGenerationError::ModuleLinkerError(e.to_string()))?;

        self.assembly_module
            .link_in_module(file.llvm_module.clone())
            .map_err(|e| CodeGenerationError::ModuleLinkerError(e.to_string()))?;

        // Function-value code is reached only through the dispatch table
        // (see `ir::closures`); once the two halves are linked its symbols
        // need not be visible outside the assembly.
        for target in group_ir.dispatch_table.fn_values().targets() {
            if let Some(function) = self.assembly_module.get_function(&target.symbol) {
                function.set_linkage(Linkage::Private);
            }
        }

        if self.code_gen.db.target().options.is_like_windows {
            // Add the useless `_fltused` symbol to indicate that the object file supports
            // floating-point values. This is required for Windows.
            let fltused =
                self.assembly_module
                    .add_global(self.code_gen.context.i32_type(), None, "_fltused");
            fltused.set_initializer(&self.code_gen.context.i32_type().const_int(1, false));
            fltused.set_linkage(Linkage::External);
        }

        let target_data = self.code_gen.target_machine.get_target_data();
        let type_context = IrTypeContext {
            context: self.code_gen.context,
            target_data: &target_data,
            struct_types: &self.code_gen.rust_types,
        };

        let value_context = IrValueContext {
            type_context: &type_context,
            context: type_context.context,
            module: &self.assembly_module,
        };

        // Build the set of dependencies
        let direct_children = module_group
            .iter()
            .flat_map(|module| module.children(self.code_gen.db))
            .collect();
        let dependencies = group_ir
            .referenced_modules
            .union(&direct_children)
            .filter_map(|&module| self.module_group_partition.group_for_module(module))
            .collect::<FxHashSet<_>>()
            .into_iter()
            .map(|group_id| {
                self.module_group_partition[group_id]
                    .relative_file_path()
                    .to_string()
            })
            .collect();

        // Generate the `get_info` method.
        symbols::gen_reflection_ir(
            self.code_gen.db,
            &value_context,
            &module_group.name,
            &file.function_definitions,
            &file.type_definitions,
            &group_ir
                .dispatch_table
                .fn_values()
                .synthetic_structs(self.code_gen.db, &self.code_gen.hir_types),
            &group_ir.dispatch_table,
            &group_ir.type_table,
            &self.code_gen.hir_types,
            self.code_gen.optimization_level,
            dependencies,
        );
        crate::hardware_safety::gen_set_hardware_safety_fn(
            self.code_gen.context,
            &self.assembly_module,
            self.code_gen.hardware_safety,
        );

        // The group's own functions are reached by direct calls and through
        // the tables `get_info` returns, never by symbol; the exported ones
        // are private already (`symbols::get_function_definition_array`).
        // The others were left with LLVM's default linkage, which in a
        // loaded library is a dynamic symbol that another library can
        // interpose: a call to a private `func error(..)` of a standard
        // library module ran the C library's `error(3)`, loaded first, and
        // crashed in `printf`. Internal linkage binds every call in the
        // assembly and keeps the symbol (in the object's local symbols).
        // A build that keeps function symbols (certifying, superoptimizing,
        // checking constant time) needs every function to stay one, as it
        // did: there hidden visibility binds the calls instead, and the
        // optimizer may not drop a function it inlined everywhere.
        // `@export` functions, the entry points the runtime looks up
        // (`DLLExport`) and the package's native code are left alone.
        for name in &file.local_symbols {
            if let Some(function) = self.assembly_module.get_function(name) {
                if function.get_linkage() == Linkage::External && function.count_basic_blocks() > 0
                {
                    if self.code_gen.keep_function_symbols {
                        function
                            .as_global_value()
                            .set_visibility(GlobalVisibility::Hidden);
                    } else {
                        function.set_linkage(Linkage::Internal);
                    }
                }
            }
        }

        // A certified build names every function's machine code by its
        // symbol. Private linkage keeps a function out of the object's
        // symbol table; internal linkage is the same to the optimizer (local
        // to the module, free to inline, specialise or drop) but keeps the
        // symbol. Declarations are left alone.
        if self.code_gen.keep_function_symbols {
            for function in self.assembly_module.get_functions() {
                if function.get_linkage() == Linkage::Private && function.count_basic_blocks() > 0 {
                    function.set_linkage(Linkage::Internal);
                }
            }
        }

        // Precise stack roots: frame records before the final optimization,
        // polls after it (see `crate::gc_roots`).
        let gc_frames = crate::gc_roots::enabled();
        if gc_frames {
            let stats = crate::gc_roots::insert_roots(&self.assembly_module);
            if std::env::var_os("CODIRA_GC_DEBUG").is_some() {
                eprintln!("gc roots: {stats:?}");
            }
        }

        // Optimize the assembly module
        optimize_module(
            &self.assembly_module,
            &self.code_gen.target_machine,
            self.code_gen.optimization_level,
        );

        if gc_frames && !matches!(std::env::var("CODIRA_GC_POLLS").as_deref(), Ok("0")) {
            let polls = crate::gc_roots::insert_polls(&self.assembly_module);
            if std::env::var_os("CODIRA_GC_DEBUG").is_some() {
                eprintln!("gc polls: {polls}");
            }
        }

        // Debug print the IR
        //println!("{}", assembly_module.print_to_string().to_string());

        Ok(Assembly::new(self.code_gen, self.assembly_module))
    }
}
