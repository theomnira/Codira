//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{cell::OnceCell, io::Write, path::Path};

use codira_target::spec;
use inkwell::targets::{FileType, TargetMachine};
use tempfile::NamedTempFile;

use crate::{code_gen::CodeGenerationError, image_link, linker};

pub struct ObjectFile {
    target: spec::Target,
    /// The object file as LLVM wrote it. The direct image writer reads it
    /// from here; only a link with LLD needs it on disk.
    obj: Vec<u8>,
    obj_file: OnceCell<NamedTempFile>,
    /// Whether the linked shared object keeps a symbol table with every
    /// function in it; see [`ObjectFile::with_symbol_table`].
    symbol_table: bool,
}

impl ObjectFile {
    /// Constructs a new object file from the specified `module` for `target`
    pub fn new(
        target: &spec::Target,
        target_machine: &TargetMachine,
        module: &inkwell::module::Module<'_>,
    ) -> Result<Self, anyhow::Error> {
        if target.options.linker_flavor == spec::LinkerFlavor::Ld {
            ctors_to_init_array(module);
        }
        let obj = target_machine
            .write_to_memory_buffer(module, FileType::Object)
            .map_err(|e| CodeGenerationError::MachineCodeError(e.to_string()))?;

        Ok(Self {
            target: target.clone(),
            obj: obj.as_slice().to_vec(),
            obj_file: OnceCell::new(),
            symbol_table: false,
        })
    }

    /// Makes the linked shared object keep a symbol table naming every
    /// function, local ones included (`codira build --certify`). ELF and
    /// Mach-O linkers keep one by default; see
    /// `linker::Linker::keep_symbol_table`. The direct image writer does not
    /// write one, so it declines such a link.
    pub fn with_symbol_table(mut self, enabled: bool) -> Self {
        self.symbol_table = enabled;
        self
    }

    /// Where the object file is on disk, writing it there first if this is
    /// the first time it is needed.
    fn on_disk(&self) -> Result<&Path, anyhow::Error> {
        if self.obj_file.get().is_none() {
            let mut file = tempfile::NamedTempFile::new()
                .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;
            file.write_all(&self.obj)
                .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;
            // Single-threaded: nothing else can have set it since the check.
            let _ = self.obj_file.set(file);
        }
        Ok(self
            .obj_file
            .get()
            .expect("the object file was just written")
            .path())
    }

    /// Returns where the object file was written.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        self.on_disk().expect("object file could be written")
    }

    /// Links the object file into a shared object.
    ///
    /// `exported_symbols` names the `@export("C")` functions that must be
    /// reachable from outside the assembly -- see
    /// `linker::Linker::build_shared_object` for why only some platforms
    /// need them spelled out.
    ///
    /// On `x86_64-pc-windows-msvc` the image is written directly by
    /// `codira_image` when it can be, and by LLD otherwise (see
    /// `image_link`).
    pub fn into_shared_object(
        self,
        output_path: &Path,
        exported_symbols: &[String],
        native: Option<&crate::NativeInputs>,
    ) -> Result<(), anyhow::Error> {
        image_link::dump_object(&self.obj, exported_symbols);
        let direct = if self.symbol_table {
            image_link::decline_without_trying(
                &self.target,
                "symbol-table",
                "the library has to keep a symbol table",
            );
            None
        } else {
            image_link::link_directly(
                &self.target,
                &self.obj,
                output_path,
                exported_symbols,
                native.is_some(),
            )
        };
        let Some(image) = direct else {
            return self.link_with_lld(output_path, exported_symbols, native);
        };

        // Self-check: have LLD link the same object and require the same
        // image, then use ours.
        if image_link::verification_enabled() {
            self.link_with_lld(output_path, exported_symbols, native)?;
            let lld = std::fs::read(output_path)?;
            image_link::verify_against(&image, &lld, output_path)?;
        }
        std::fs::write(output_path, &image)?;
        Ok(())
    }

    fn link_with_lld(
        &self,
        output_path: &Path,
        exported_symbols: &[String],
        native: Option<&crate::NativeInputs>,
    ) -> Result<(), anyhow::Error> {
        // Construct a linker for the target
        let mut linker = linker::create_with_target(&self.target);
        linker.add_object(self.on_disk()?)?;
        if let Some(native) = native {
            linker.add_native(native)?;
        }

        if self.symbol_table {
            linker.keep_symbol_table();
        }

        // Link the object
        linker.build_shared_object(output_path, exported_symbols)?;
        linker.finalize()?;

        Ok(())
    }
}

/// What the C runtime's start files (`crtbegin.o`) give an ELF shared
/// object, for the C and C++ code `[native]` links into a Codira library.
///
/// A target machine made through LLVM's C API emits static constructors and
/// destructors (`@llvm.global_ctors`/`@llvm.global_dtors`: C++ globals with
/// a dynamic initialiser, `__attribute__((constructor))`) as `.ctors` and
/// `.dtors` sections; it has no switch for `.init_array`, which Clang turns
/// on itself. A Codira library is linked without the start files, and the
/// dynamic loader runs only `DT_INIT_ARRAY`/`DT_FINI_ARRAY`, so such a
/// constructor never ran (a C++ `static std::vector` stayed empty).
///
/// So, as `crtbegin.o` and `.init_array` emission would:
///
/// * the constructors, ordered by priority (ascending, stable for equal
///   priorities, since the loader runs the array in address order), become
///   one pointer array in `.init_array`, and the destructors one in
///   `.fini_array` (which the loader runs in reverse);
/// * a module that refers to `__dso_handle` -- C++ registers each static
///   object's destructor with `__cxa_atexit(dtor, obj, &__dso_handle)` --
///   gets a hidden definition of it, and a `.fini_array` entry that calls
///   `__cxa_finalize(&__dso_handle)`, so those destructors run when the
///   library is unloaded, while its code is still mapped. Without it they
///   ran at process exit, after the library was gone, and the process
///   crashed in `exit`.
///
/// LLVM's ELF writer gives the two sections their `SHT_INIT_ARRAY` and
/// `SHT_FINI_ARRAY` types from their names. The entries' associated-data
/// field (COMDAT deduplication) is not needed: every module contributes its
/// own constructors exactly once. A module with none of these is unchanged.
fn ctors_to_init_array(module: &inkwell::module::Module<'_>) {
    use inkwell::values::AsValueRef;
    use llvm_sys::{core, prelude::LLVMValueRef};

    // SAFETY: plain queries and constructions on a module and context that
    // outlive this call; every value passed is one LLVM just returned.
    unsafe {
        let module_ref = module.as_mut_ptr();
        let context = core::LLVMGetModuleContext(module_ref);
        let pointer = core::LLVMPointerTypeInContext(context, 0);

        // The functions of `@llvm.global_ctors`/`@llvm.global_dtors`,
        // ordered by priority; the global itself is removed.
        let take = |name: &std::ffi::CStr| -> Vec<LLVMValueRef> {
            let Some(global) = module.get_global(name.to_str().unwrap_or_default()) else {
                return Vec::new();
            };
            let global = global.as_value_ref();
            let init = core::LLVMGetInitializer(global);
            let mut entries: Vec<(u64, LLVMValueRef)> = Vec::new();
            if !init.is_null() {
                let count = u32::try_from(core::LLVMGetNumOperands(init)).unwrap_or(0);
                for i in 0..count {
                    let entry = core::LLVMGetOperand(init, i);
                    if core::LLVMGetNumOperands(entry) < 2 {
                        continue;
                    }
                    let priority = core::LLVMConstIntGetZExtValue(core::LLVMGetOperand(entry, 0));
                    let function = core::LLVMGetOperand(entry, 1);
                    if core::LLVMIsNull(function) == 0 {
                        entries.push((priority, function));
                    }
                }
            }
            core::LLVMDeleteGlobal(global);
            entries.sort_by_key(|(priority, _)| *priority);
            entries.into_iter().map(|(_, f)| f).collect()
        };
        let constructors = take(c"llvm.global_ctors");
        let mut destructors = take(c"llvm.global_dtors");

        // `__dso_handle`, defined as `crtbegin.o` defines it in a shared
        // object (its own address), and its finaliser.
        let dso_handle = core::LLVMGetNamedGlobal(module_ref, c"__dso_handle".as_ptr());
        if !dso_handle.is_null() && core::LLVMGetInitializer(dso_handle).is_null() {
            // Only its address matters: it names this library to
            // `__cxa_atexit` and `__cxa_finalize`.
            let value_type = core::LLVMGlobalGetValueType(dso_handle);
            core::LLVMSetInitializer(dso_handle, core::LLVMConstNull(value_type));
            core::LLVMSetVisibility(dso_handle, llvm_sys::LLVMVisibility::LLVMHiddenVisibility);

            let void = core::LLVMVoidTypeInContext(context);
            let mut finalize_params = [pointer];
            let finalize_type = core::LLVMFunctionType(void, finalize_params.as_mut_ptr(), 1, 0);
            let mut finalize = core::LLVMGetNamedFunction(module_ref, c"__cxa_finalize".as_ptr());
            if finalize.is_null() {
                finalize =
                    core::LLVMAddFunction(module_ref, c"__cxa_finalize".as_ptr(), finalize_type);
            }
            let fini_type = core::LLVMFunctionType(void, std::ptr::null_mut(), 0, 0);
            let fini = core::LLVMAddFunction(module_ref, c"codira.dso_fini".as_ptr(), fini_type);
            core::LLVMSetLinkage(fini, llvm_sys::LLVMLinkage::LLVMInternalLinkage);
            let entry = core::LLVMAppendBasicBlockInContext(context, fini, c"entry".as_ptr());
            let builder = core::LLVMCreateBuilderInContext(context);
            core::LLVMPositionBuilderAtEnd(builder, entry);
            let mut args = [dso_handle];
            core::LLVMBuildCall2(
                builder,
                finalize_type,
                finalize,
                args.as_mut_ptr(),
                1,
                c"".as_ptr(),
            );
            core::LLVMBuildRetVoid(builder);
            core::LLVMDisposeBuilder(builder);
            // First in `.fini_array`, which the loader runs in reverse: it
            // runs after every destructor of `@llvm.global_dtors`.
            destructors.insert(0, fini);
        }

        let mut kept = Vec::new();
        for (functions, name, section) in [
            (constructors, c"codira.init_array", c".init_array"),
            (destructors, c"codira.fini_array", c".fini_array"),
        ] {
            if functions.is_empty() {
                continue;
            }
            let mut functions = functions;
            let length = u64::try_from(functions.len()).unwrap_or(u64::MAX);
            let array_type = core::LLVMArrayType2(pointer, length);
            let array = core::LLVMConstArray2(pointer, functions.as_mut_ptr(), length);
            let global = core::LLVMAddGlobal(module_ref, array_type, name.as_ptr());
            core::LLVMSetInitializer(global, array);
            core::LLVMSetLinkage(global, llvm_sys::LLVMLinkage::LLVMInternalLinkage);
            core::LLVMSetGlobalConstant(global, 1);
            core::LLVMSetSection(global, section.as_ptr());
            core::LLVMSetAlignment(global, 8);
            kept.push(global);
        }

        // Nothing references the arrays; `@llvm.compiler.used` keeps them
        // through any later pass. The module has none of its own (Codira
        // emits none, and a `[native]` one would have been appended into the
        // module's when the bitcode was linked), so it is created here.
        if !kept.is_empty()
            && core::LLVMGetNamedGlobal(module_ref, c"llvm.compiler.used".as_ptr()).is_null()
        {
            let length = u64::try_from(kept.len()).unwrap_or(u64::MAX);
            let used_type = core::LLVMArrayType2(pointer, length);
            let used_array = core::LLVMConstArray2(pointer, kept.as_mut_ptr(), length);
            let used_global =
                core::LLVMAddGlobal(module_ref, used_type, c"llvm.compiler.used".as_ptr());
            core::LLVMSetInitializer(used_global, used_array);
            core::LLVMSetLinkage(used_global, llvm_sys::LLVMLinkage::LLVMAppendingLinkage);
            core::LLVMSetSection(used_global, c"llvm.metadata".as_ptr());
        }
    }
}
