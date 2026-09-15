//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
pub use inkwell::{builder::Builder, context::Context, module::Module, OptimizationLevel};

pub use crate::{
    assembly::{AssemblyIr, TargetAssembly},
    code_gen::AssemblyBuilder,
    db::{CodeGenDatabase, CodeGenDatabaseStorage},
    module_group::ModuleGroup,
    module_partition::{ModuleGroupId, ModulePartition},
};

/// This library generates machine code from HIR using inkwell which is a safe
/// wrapper around LLVM.
mod code_gen;
mod db;
mod eidos_fold;
#[macro_use]
mod ir;
mod assembly;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod test;

pub mod value;

mod apple;
pub(crate) mod intrinsics;
mod linker;
pub mod mir_codegen;
mod module_group;
mod module_partition;
pub(crate) mod type_info;
