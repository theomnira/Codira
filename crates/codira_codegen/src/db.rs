//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{rc::Rc, sync::Arc};

use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetTriple};

use crate::{AssemblyIr, ModuleGroupId, ModulePartition, TargetAssembly};

/// The `CodeGenDatabase` enables caching of code generation stages.
/// Inkwell/LLVM objects are not stored in the cache because they are not
/// thread-safe.
///
/// The main purpose of using this Salsa database is to enable caching of
/// high-level objects based on changes to source files. Although the code
/// generation cache is pretty granular there is still a benefit to not having
/// to recompile assemblies if not required.
///
/// Deliberately *not* a `#[salsa::database]`-parallel-safe database: nothing
/// here stops it from being one except history, and the one thing that used
/// to (a memoized `Rc<TargetMachine>` -- see `build_target_machine` below)
/// no longer lives on this trait. `ParallelDatabase::snapshot` requires the
/// whole database, and every query's key and value, to be `Send`
/// (salsa 0.16's own doc comment on the trait says so directly); an `Rc` is
/// never `Send`, so caching one here would have permanently blocked
/// `Driver::write_all_assemblies` from ever compiling independent module
/// groups on separate threads -- which is exactly what it now does, since
/// `build_partition` gives each module its own group and each group's
/// codegen touches no other group's state.
#[salsa::query_group(CodeGenDatabaseStorage)]
pub trait CodeGenDatabase: codira_hir::HirDatabase {
    /// Set the optimization level used to generate assemblies
    #[salsa::input]
    fn optimization_level(&self) -> inkwell::OptimizationLevel;

    /// The build mode and the e-graph bounds Eidos runs under (see
    /// [`crate::EidosOptions`]).
    #[salsa::input]
    fn eidos_options(&self) -> crate::EidosOptions;

    /// Whether generated code decrements a fuel counter at every function
    /// entry and loop back-edge (`--fuel`), so a runtime can bound how long
    /// a call may run.
    #[salsa::input]
    fn fuel_instrumentation(&self) -> bool;

    /// Whether every function keeps a symbol in the assembly's symbol table
    /// (`codira build --certify`): functions get internal instead of private
    /// linkage, which changes nothing about the code LLVM generates, and the
    /// COFF linker writes a symbol table it otherwise leaves out. A proof
    /// certificate names each function's machine code by these symbols.
    #[salsa::input]
    fn keep_function_symbols(&self) -> bool;

    /// Whether class instances and short array literals that the escape
    /// analysis proves never outlive their call are placed in the stack
    /// frame instead of the GC heap (`--frame-alloc`; see
    /// `codira_hir::escape`). Experimental, off by default.
    #[salsa::input]
    fn frame_allocation(&self) -> bool;

    /// The experimental hardware-safety mode generated code is built with
    /// (`--sanitize=memtag`; see [`crate::HardwareSafety`]).
    #[salsa::input]
    fn hardware_safety(&self) -> crate::HardwareSafety;
    /// The package's native code (C, C++, assembly), linked into every module
    /// group that calls it. See `crate::native`.
    #[salsa::input]
    fn native_inputs(&self) -> Arc<crate::NativeInputs>;

    /// Returns the current module partition
    #[salsa::invoke(crate::module_partition::build_partition)]
    fn module_partition(&self) -> Arc<ModulePartition>;

    /// Returns a file containing the IR for the specified module.
    #[salsa::invoke(crate::assembly::build_assembly_ir)]
    fn assembly_ir(&self, module_group: ModuleGroupId) -> Arc<AssemblyIr>;

    /// The package's `@rewrite` rules: those `codira_tv` proved, and why the
    /// others were refused (see `crate::rewrite`).
    #[salsa::invoke(crate::rewrite::rewrite_rules_query)]
    fn rewrite_rules(&self) -> Arc<crate::rewrite::RewriteRules>;

    /// PTX for the group's `@target(gpu)` functions, `None` when it has none,
    /// or why they cannot run on a GPU (see `crate::gpu`).
    #[salsa::invoke(crate::gpu::gpu_kernels_query)]
    fn gpu_kernels(
        &self,
        module_group: ModuleGroupId,
    ) -> Arc<Result<Option<crate::gpu::GpuKernels>, String>>;

    /// Returns a fully linked shared object for the specified module.
    #[salsa::invoke(crate::assembly::build_target_assembly)]
    fn target_assembly(&self, module_group: ModuleGroupId) -> Arc<TargetAssembly>;
}

/// Constructs the complete machine description for the code generation
/// target. All target-specific information should be accessible through
/// this interface.
///
/// A plain function, called once per `CodeGenContext`, not a memoized salsa
/// query: `TargetMachine` wraps a raw LLVM pointer (`NonNull`, which is
/// never `Send`), so caching one as a query result would make it a field of
/// the database itself and block the database from ever being safe to use
/// from more than one thread. Constructing it is cheap relative to the
/// optimisation pass it configures -- the LLVM data-layout/CPU-feature
/// lookup this does costs microseconds; `optimize_module`'s pass pipeline
/// costs low-single-digit milliseconds even for a near-empty module (see
/// `phase_timings`) -- so paying it once per module group rather than once
/// per process is not a meaningful cost, and it is what makes each group's
/// `TargetMachine` a value that belongs to exactly one thread for its
/// entire lifetime, which is the only thread-safety contract LLVM actually
/// promises for this type.
pub(crate) fn build_target_machine(
    db: &dyn CodeGenDatabase,
) -> Rc<inkwell::targets::TargetMachine> {
    // Get the HIR target
    let target = db.target();

    initialize_target_backends_once();

    // Retrieve the LLVM target using the specified target.
    let target_triple = TargetTriple::create(&db.target().llvm_target);
    let llvm_target = Target::from_triple(&target_triple)
        .expect("could not find llvm target tripple for Codira target");

    // Construct target machine for machine code generation
    let target_machine = llvm_target
        .create_target_machine(
            &target_triple,
            &target.options.cpu,
            &target.options.features,
            db.optimization_level(),
            // A WebAssembly module is linked statically: position-independent
            // wasm code addresses memory through imported `__memory_base`/GOT
            // globals, which a freestanding module has no host to provide.
            if crate::wasm::is_wasm_target(&target) {
                RelocMode::Static
            } else {
                RelocMode::PIC
            },
            CodeModel::Default,
        )
        .expect("could not create llvm target machine");

    Rc::new(target_machine)
}

/// Runs LLVM's target-backend initialization exactly once per process.
///
/// `build_target_machine` now runs once per module group instead of once
/// per process (see its own doc comment), and with parallel codegen that
/// can mean several threads calling it around the same time. LLVM's
/// `Target::initialize_*` functions set up global, static backend state;
/// nothing in their contract promises that two threads calling them for
/// the first time concurrently is safe, so a `std::sync::Once` -- not a
/// per-call guard -- is what actually keeps that a single, ordered event
/// regardless of how many threads reach this function.
pub(crate) fn initialize_target_backends_once() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        #[cfg(feature = "target-x86")]
        Target::initialize_x86(&InitializationConfig::default());
        #[cfg(feature = "target-aarch64")]
        Target::initialize_aarch64(&InitializationConfig::default());
        #[cfg(feature = "target-riscv")]
        Target::initialize_riscv(&InitializationConfig::default());
        #[cfg(feature = "target-webassembly")]
        Target::initialize_webassembly(&InitializationConfig::default());
        #[cfg(feature = "target-nvptx")]
        Target::initialize_nvptx(&InitializationConfig::default());
    });
}
