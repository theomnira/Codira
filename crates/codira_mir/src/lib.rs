//! Copyright (c) 2026 Omnira CJSC
//!
//! `codira_mir` -- the **Eidos** parametric mid-level IR.
//!
//! Eidos (εἶδος, Plato's "Form") is Codira's mid-level
//! compilation framework: this IR, the elaborator and interpreter in
//! `codira_comptime`, the equality-saturation optimizer in
//! `codira_egraph`, and the translation validator in `codira_tv`. A
//! generator is the Form; elaboration produces its instances.
//!
//! Sits between `codira_hir` and `codira_codegen`, modeled on (and
//! extending) the pipeline shape of Modular's KGEN (the Mojo compiler).
//! See `spec/EIDOS_ARCHITECTURE.md` for the full design rationale
//! and `spec/KGEN_SUPERSET_STATUS.md` for exactly what is implemented vs.
//! designed as of a given session.
//!
//! Layout:
//! * `op` -- the IR data model: region-structured SSA with block arguments,
//!   loop-carried values, and explicit `cf.yield` (MLIR `scf`-style, matching
//!   KGEN's `hlcf` dialect -- see `op`'s module doc for why structured control
//!   flow rather than a flat CFG).
//! * `generator` -- parametric function/struct templates (`kgen.generator`
//!   analogs); elaboration lives in `codira_comptime`.
//! * `verify` -- the structural verifier (KGEN's `kgen-verifier` analog).
//! * `print` -- MLIR-flavored textual form, for humans and snapshot tests.
//! * `pass` -- the pass manager and the optimization passes that run between
//!   elaboration and codegen (KGEN's `Transforms/` analog).
//!
//! Compile-time evaluation/elaboration lives in the separate
//! `codira_comptime` crate, which depends on this one -- kept separate so
//! the IR data model has no evaluation-strategy dependencies.

pub mod fold;
mod generator;
mod op;
pub mod pass;
pub mod print;
pub mod ty;
pub mod verify;

pub use fold::{fold_cast, fold_op, FoldError};
pub use generator::{
    Generator, GeneratorId, GeneratorParam, GeneratorStore, StructGenerator, StructGeneratorId,
};
pub use op::{Attr, Body, CastKind, CastMode, Op, OpId, OpKind, Region};
pub use print::print_body;
pub use ty::{Type, TypeId, TypeStore};
pub use verify::{check_types, verify_body, TypeError, VerifyError};

#[cfg(test)]
mod tests;
