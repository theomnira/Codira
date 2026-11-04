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
    eidos_options::{BuildMode, EidosOptions, EidosRun},
    hardware_safety::HardwareSafety,
    image_link::{
        image_link_stats, reset_image_link_stats, verification_enabled, ImageLinkStats,
        DIRECT_ELF_ENV, DUMP_OBJECTS_ENV, LINK_LOG_ENV, LINK_VERIFY_ENV,
    },
    linker::{in_process_lld_available, link_mode, LinkMode, DIRECT_LINK_ENV, LINK_MODE_ENV},
    module_group::ModuleGroup,
    module_partition::{ModuleGroupId, ModulePartition},
    native::NativeInputs,
    target_cpu::{resolve_target_cpu, TargetCpuError},
    wasm::is_wasm_target,
};

/// This library generates machine code from HIR using inkwell which is a safe
/// wrapper around LLVM.
mod code_gen;
mod db;
mod eidos_fold;
mod eidos_options;
pub mod rewrite;
#[macro_use]
mod ir;
mod assembly;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod run_test;
#[cfg(test)]
mod test;
#[cfg(test)]
mod test_memtag;
#[cfg(test)]
mod test_targets;
#[cfg(test)]
mod test_never;

pub mod value;

mod apple;
pub mod gpu;
pub mod hardware_safety;
mod image_link;
pub(crate) mod intrinsics;
mod linker;
pub mod mir_codegen;
mod module_group;
mod module_partition;
mod native;
mod target_cpu;
pub(crate) mod type_info;
mod wasm;
