//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use std::{cell::RefCell, collections::HashMap, rc::Rc};

use inkwell::{context::Context, module::Module, targets::TargetMachine, types::StructType};

use crate::{ir::ty::HirTypeCache, CodeGenDatabase};

pub struct CodeGenContext<'db, 'ink> {
    /// The current LLVM context
    pub context: &'ink Context,

    /// The Salsa HIR database
    pub db: &'db dyn codira_hir::HirDatabase,

    /// A mapping from Rust types' full path (without lifetime) to inkwell types
    pub rust_types: RefCell<HashMap<&'static str, StructType<'ink>>>,

    /// A mapping from HIR types to LLVM struct types
    pub hir_types: HirTypeCache<'db, 'ink>,

    /// The optimization level
    pub optimization_level: inkwell::OptimizationLevel,

    /// The target to generate code for
    pub target_machine: Rc<TargetMachine>,

    /// The e-graph optimizer Eidos runs with, or `None` when this build
    /// skips Eidos (`--mode=dev`). Read once from the salsa inputs here so
    /// every function body of the group sees the same answer.
    pub eidos: Option<codira_egraph::EGraphOptimizer>,

    /// The same database as `db`, seen as a `CodeGenDatabase`, for the
    /// queries that only code generation answers (the package's proven
    /// rewrite rules, read when bodies are generated -- see
    /// [`CodeGenContext::eidos_run`]).
    pub codegen_db: &'db dyn CodeGenDatabase,
    /// The package's native code (see `crate::native`)
    pub native: std::sync::Arc<crate::NativeInputs>,
    /// Whether generated code is fuel-instrumented (`--fuel`).
    pub fuel: bool,
    /// Whether every function keeps its symbol (`--certify`).
    pub keep_function_symbols: bool,
    /// The experimental hardware-safety mode (see `crate::hardware_safety`).
    pub hardware_safety: crate::HardwareSafety,
    /// Whether non-escaping objects go to the stack frame
    /// ([`crate::CodeGenDatabase::frame_allocation`]).
    pub frame_allocation: bool,
    /// What generating code under `hardware_safety` refused, as
    /// `file:line:column: reason` lines; a non-empty list fails the build.
    pub safety_refusals: std::rc::Rc<RefCell<Vec<String>>>,
}

impl<'db, 'ink> CodeGenContext<'db, 'ink> {
    /// Constructs a new `CodeGenContext` from an LLVM context and a
    /// `CodeGenDatabase`.
    pub fn new(context: &'ink Context, db: &'db dyn CodeGenDatabase) -> Self {
        let target_machine = crate::db::build_target_machine(db);
        let hardware_safety = db.hardware_safety();
        Self {
            context,
            rust_types: RefCell::new(HashMap::default()),
            hir_types: HirTypeCache::new(context, db, target_machine.get_target_data()),
            optimization_level: db.optimization_level(),
            target_machine,
            // Eidos folds whole functions to constants by interpreting them,
            // memory included; a hardware-safety build must keep every access
            // as the checked instructions it asked for, so it skips Eidos.
            eidos: if hardware_safety.is_enabled() {
                None
            } else {
                db.eidos_options().optimizer(&db.target())
            },
            codegen_db: db,
            native: db.native_inputs(),
            fuel: db.fuel_instrumentation(),
            keep_function_symbols: db.keep_function_symbols(),
            // ARM MTE tags `stack_alloc` slots and checks every access; a
            // frame object is untagged stack memory, so the two modes are
            // not combined.
            frame_allocation: db.frame_allocation() && !hardware_safety.is_enabled(),
            hardware_safety,
            safety_refusals: std::rc::Rc::default(),
            db,
        }
    }

    /// What Eidos runs with while this context generates function bodies:
    /// the optimizer and the package's proven rewrite rules, or `None` when
    /// the build skips Eidos. The rules are read here rather than in
    /// [`CodeGenContext::new`], which also serves contexts that generate no
    /// bodies (and databases with no packages to read rules from).
    pub fn eidos_run(&self) -> Option<crate::EidosRun> {
        let optimizer = self.eidos?;
        Some(crate::EidosRun {
            optimizer,
            rules: std::sync::Arc::from(self.codegen_db.rewrite_rules().admitted.clone()),
        })
    }

    /// Constructs a new `Module` with the specified name and initialized for
    /// the target.
    pub fn create_module(&self, name: impl AsRef<str>) -> Module<'ink> {
        let module = self.context.create_module(name.as_ref());
        module.set_data_layout(&self.target_machine.get_target_data().get_data_layout());
        module.set_triple(&self.target_machine.get_triple());
        module
    }
}
