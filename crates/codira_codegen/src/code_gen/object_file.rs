//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{io::Write, path::Path};

use codira_target::spec;
use inkwell::targets::{FileType, TargetMachine};
use tempfile::NamedTempFile;

use crate::{code_gen::CodeGenerationError, linker};

pub struct ObjectFile {
    target: spec::Target,
    obj_file: NamedTempFile,
}

impl ObjectFile {
    /// Constructs a new object file from the specified `module` for `target`
    pub fn new(
        target: &spec::Target,
        target_machine: &TargetMachine,
        module: &inkwell::module::Module<'_>,
    ) -> Result<Self, anyhow::Error> {
        let obj = target_machine
            .write_to_memory_buffer(module, FileType::Object)
            .map_err(|e| CodeGenerationError::MachineCodeError(e.to_string()))?;

        let mut obj_file = tempfile::NamedTempFile::new()
            .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;
        obj_file
            .write(obj.as_slice())
            .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;

        Ok(Self {
            target: target.clone(),
            obj_file,
        })
    }

    /// Links the object file into a shared object.
    ///
    /// `exported_symbols` names the `@export("C")` functions that must be
    /// reachable from outside the assembly -- see
    /// `linker::Linker::build_shared_object` for why only some platforms
    /// need them spelled out.
    pub fn into_shared_object(
        self,
        output_path: &Path,
        exported_symbols: &[String],
    ) -> Result<(), anyhow::Error> {
        // Construct a linker for the target
        let mut linker = linker::create_with_target(&self.target);
        linker.add_object(self.obj_file.path())?;

        // Link the object
        linker.build_shared_object(output_path, exported_symbols)?;
        linker.finalize()?;

        Ok(())
    }
}
