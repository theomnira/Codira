//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
pub use assembly_builder::AssemblyBuilder;
pub use context::CodeGenContext;
pub use error::CodeGenerationError;
use inkwell::{
    module::Module, passes::PassBuilderOptions, targets::TargetMachine, OptimizationLevel,
};
pub(crate) use object_file::ObjectFile;

mod assembly_builder;
mod context;
mod error;
mod object_file;
pub mod symbols;

/// Optimizes the specified LLVM `Module` using the default passes for the given
/// `OptimizationLevel`.
///
/// Uses LLVM's "new" pass manager (`Module::run_passes` + a named pipeline
/// string) rather than the legacy `PassManager`/`PassManagerBuilder` API --
/// the entire legacy pass-adding API (including every individual
/// `PassManager::add_*_pass` method, not just the builder) is compiled out
/// of inkwell for LLVM 17+ (`#[llvm_versions(..=16)]` throughout
/// `inkwell::passes`), so there is no legacy-API path left on LLVM 22 at
/// all. The new API is also module-level only (no separate per-function
/// pass manager/`run_on(&fn_value)` concept), which is why callers that
/// used to run passes function-by-function now call this once over the
/// whole module instead -- see `ir::function`'s (removed)
/// `create_pass_manager` callers.
pub(crate) fn optimize_module(
    module: &Module<'_>,
    target_machine: &TargetMachine,
    optimization_lvl: OptimizationLevel,
) {
    let passes = match optimization_lvl {
        OptimizationLevel::None => "default<O0>",
        OptimizationLevel::Less => "default<O1>",
        OptimizationLevel::Default => "default<O2>",
        OptimizationLevel::Aggressive => "default<O3>",
    };
    module
        .run_passes(passes, target_machine, PassBuilderOptions::create())
        .expect("failed to run optimization passes");
}
