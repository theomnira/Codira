//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{path::Path, sync::Arc};

use anyhow::anyhow;
use inkwell::context::Context;
use tempfile::NamedTempFile;

use crate::{
    code_gen::{AssemblyBuilder, CodeGenContext, ObjectFile},
    db::CodeGenDatabase,
    ModuleGroupId,
};

/// An `Assembly` is a successfully linked module of code from one or more
/// files.
pub struct Assembly<'db, 'ink, 'ctx> {
    code_gen: &'ctx CodeGenContext<'db, 'ink>,
    module: inkwell::module::Module<'ink>,
}

impl<'db, 'ink, 'ctx> Assembly<'db, 'ink, 'ctx> {
    /// Constructs an assembly
    pub fn new(
        code_gen: &'ctx CodeGenContext<'db, 'ink>,
        module: inkwell::module::Module<'ink>,
    ) -> Self {
        Self { code_gen, module }
    }

    /// Whether this assembly calls or contains any of `native`'s functions,
    /// and so has to be linked with the package's native code.
    pub fn uses_native_code(&self, native: &crate::NativeInputs) -> bool {
        !native.is_empty()
            && self.module.get_functions().any(|function| {
                function
                    .get_name()
                    .to_str()
                    .is_ok_and(|name| native.symbols.contains(name))
            })
    }

    /// Tries to convert the assembly into an `ObjectFile`.
    pub fn into_object_file(self) -> Result<ObjectFile, anyhow::Error> {
        ObjectFile::new(
            &self.code_gen.db.target(),
            &self.code_gen.target_machine,
            &self.module,
        )
    }

    /// Tries to write the `Assembly`'s IR to file.
    pub fn write_ir_to_file(self, output_path: &Path) -> Result<(), anyhow::Error> {
        self.module
            .print_to_file(output_path)
            .map_err(|e| anyhow!("{e}"))
    }
}

/// Builds an assembly for the specified file
fn build_assembly<'db, 'ink, 'ctx>(
    db: &'db dyn CodeGenDatabase,
    code_gen: &'ctx CodeGenContext<'db, 'ink>,
    module_group_id: ModuleGroupId,
) -> Assembly<'db, 'ink, 'ctx> {
    // Setup the code generation context
    let module_partition = db.module_partition();

    let module_builder = AssemblyBuilder::new(code_gen, &module_partition, module_group_id);
    module_builder.build().expect("unable to create assembly")
}

/// A `TargetAssembly` is a reference to a Codira library stored on disk --
/// or, for a target that can refuse the code it is given (WebAssembly, see
/// `crate::wasm`), the reason there is none.
#[derive(Debug)]
pub struct TargetAssembly {
    file: Result<NamedTempFile, String>,
    extension: &'static str,
}

impl PartialEq for TargetAssembly {
    fn eq(&self, other: &Self) -> bool {
        match (&self.file, &other.file) {
            (Ok(a), Ok(b)) => a.path() == b.path(),
            (Err(a), Err(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for TargetAssembly {}

impl TargetAssembly {
    pub const EXTENSION: &'static str = "codiralib";

    /// Returns the current location of the assembly; empty when it could
    /// not be built (see [`TargetAssembly::error`]).
    pub fn path(&self) -> &Path {
        self.file
            .as_ref()
            .map_or(Path::new(""), NamedTempFile::path)
    }

    /// Why this target refused to build the assembly, if it did.
    pub fn error(&self) -> Option<&str> {
        self.file.as_ref().err().map(String::as_str)
    }

    /// The extension of the output file: `codiralib` for a native library,
    /// `wasm` for a WebAssembly module.
    pub fn extension(&self) -> &'static str {
        self.extension
    }

    /// Copies the assembly to the specified location.
    ///
    /// The copy is written to a temporary file in the destination's
    /// directory and renamed onto the destination, so the runtime, which
    /// watches that directory and loads what it finds, can never read a file
    /// that is half written. See [`replace_file`].
    pub fn copy_to<P: AsRef<Path>>(&self, destination: P) -> Result<(), std::io::Error> {
        match &self.file {
            Ok(file) => replace_file(file.path(), destination.as_ref()),
            Err(reason) => Err(std::io::Error::other(reason.clone())),
        }
    }
}

/// Makes `destination` a copy of `source` in one step: the copy is written
/// beside the destination under a name the runtime ignores
/// ([`codira_abi::OUTPUT_TEMP_EXTENSION`]), then renamed over it.
///
/// A rename onto a file can fail on Windows while another process has the
/// destination open without sharing deletes (a virus scanner, a loader in
/// the middle of mapping it). That passes, so the rename is retried for a
/// short while; if it never succeeds the destination is overwritten in
/// place, which is what this function replaced, and the temporary file is
/// removed either way.
fn replace_file(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = destination.with_file_name(format!(
        ".{name}.{}-{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        codira_abi::OUTPUT_TEMP_EXTENSION
    ));
    std::fs::copy(source, &temporary)?;

    let mut result = std::fs::rename(&temporary, destination);
    for attempt in 0..20 {
        if result.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1 + attempt));
        result = std::fs::rename(&temporary, destination);
    }
    if result.is_err() {
        let overwritten = std::fs::copy(&temporary, destination).map(|_| ());
        let _ = std::fs::remove_file(&temporary);
        return overwritten;
    }
    Ok(())
}

/// Builds an assembly for the specified module.
pub(crate) fn build_target_assembly(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
) -> Arc<TargetAssembly> {
    // A WebAssembly module is not a `.codiralib`; it is built, or refused,
    // by its own path.
    if crate::wasm::is_wasm_target(&db.target()) {
        return Arc::new(TargetAssembly {
            file: crate::wasm::build_wasm_module(db, module_group),
            extension: crate::wasm::EXTENSION,
        });
    }

    // An experimental hardware-safety mode the target cannot honour is the
    // build's error, before any code is generated for it.
    let safety = db.hardware_safety();
    if let Err(reason) = crate::hardware_safety::validate(safety, &db.target()) {
        return Arc::new(TargetAssembly {
            file: Err(reason),
            extension: TargetAssembly::EXTENSION,
        });
    }

    // Setup the code generation context
    let inkwell_context = Context::create();
    let code_gen_context = CodeGenContext::new(&inkwell_context, db);

    // Build an assembly for the module
    let assembly = build_assembly(db, &code_gen_context, module_group);

    // Code the mode could not protect fails the build, with every place.
    let refusals = code_gen_context.safety_refusals.borrow().clone();
    if !refusals.is_empty() {
        return Arc::new(TargetAssembly {
            file: Err(refusals.join(
                "
",
            )),
            extension: TargetAssembly::EXTENSION,
        });
    }

    // The package's native code goes into every assembly that calls it.
    let native = db.native_inputs();
    let native = assembly.uses_native_code(&native).then_some(&*native);

    // Convert the assembly into an object file
    let obj_file = assembly
        .into_object_file()
        .expect("unable to create object file")
        .with_symbol_table(db.keep_function_symbols());

    // Construct a temporary file for the assembly
    let file = NamedTempFile::new().expect("could not create temp file for shared object");

    // Every `@export("C")` function in this group must survive into the
    // assembly's export table; see `linker::Linker::build_shared_object`.
    let module_partition = db.module_partition();
    let mut exported_symbols: Vec<String> = module_partition[module_group]
        .iter()
        .flat_map(|module| module.all_functions(db))
        .filter(|f| f.export_abi(db).as_deref() == Some("C"))
        .map(|f| f.name(db).to_string())
        .collect();
    // A fuel-instrumented assembly also exports the hook the runtime uses to
    // point it at its fuel counter.
    if db.fuel_instrumentation() {
        exported_symbols.push(codira_abi::SET_FUEL_COUNTER_FN_NAME.to_owned());
    }
    // So does one built with a hardware-safety mode, for the hook that tells
    // the runtime which.
    if safety.is_enabled() {
        exported_symbols.push(codira_abi::SET_HARDWARE_SAFETY_FN_NAME.to_owned());
    }

    // Translate the object file into a shared object
    obj_file
        .into_shared_object(file.path(), &exported_symbols, native)
        .expect("could not link object file");

    // macOS libraries carry an ad-hoc code signature, written by LLD itself
    // (`-adhoc_codesign`, see the Mach-O linker), so the file needs no
    // further step here.

    Arc::new(TargetAssembly {
        file: Ok(file),
        extension: TargetAssembly::EXTENSION,
    })
}

/// An `AssemblyIr` is a reference to an IR file stored on disk.
#[derive(Debug)]
pub struct AssemblyIr {
    file: NamedTempFile,
}

impl PartialEq for AssemblyIr {
    fn eq(&self, other: &Self) -> bool {
        self.path().eq(other.path())
    }
}

impl Eq for AssemblyIr {}

impl AssemblyIr {
    pub const EXTENSION: &'static str = "ll";

    /// Returns the current location of the IR File.
    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// Copies the assembly to the specified location
    pub fn copy_to<P: AsRef<Path>>(&self, destination: P) -> Result<(), std::io::Error> {
        std::fs::copy(self.path(), destination).map(|_| ())
    }
}

/// Builds an IR file for the specified module.
pub(crate) fn build_assembly_ir(
    db: &dyn CodeGenDatabase,
    module_group: ModuleGroupId,
) -> Arc<AssemblyIr> {
    // Setup the code generation context
    let inkwell_context = Context::create();
    let code_gen_context = CodeGenContext::new(&inkwell_context, db);

    // Build an assembly for the module
    let assembly = build_assembly(db, &code_gen_context, module_group);

    // Construct a temporary file for the assembly
    let file = NamedTempFile::new().expect("could not create temp file for shared object");

    // Write the assembly's IR to disk
    assembly
        .write_ir_to_file(file.path())
        .expect("could not write to temp file");

    Arc::new(AssemblyIr { file })
}
