//! Copyright (c) 2026 Omnira CJSC
//!
//! `codira_comptime` -- the parametric compile-time evaluator for
//! `codira_mir`. Structural analog of `KGEN/lib/Elaborator` +
//! `KGEN/lib/Interpreter`. See `spec/EIDOS_ARCHITECTURE.md` §4.
//!
//! This crate provides:
//! * the interpreter (§4 point 1) -- `eval_body`/`eval_body_with`/ [`EvalCtx`],
//!   a full evaluator for the region-structured IR including loops
//!   (`cf.while`/`cf.for` with loop-carried values), tuples, and `core.call`
//!   resolution against a `GeneratorStore`, with fuel and call-depth limits so
//!   comptime evaluation always terminates;
//! * the elaborator (§4 point 2) -- `elaborate`, generator
//!   specialization/monomorphization with constant folding via
//!   `codira_mir::fold_op` (wired as a salsa query in `codira_hir` for
//!   incremental reuse per §4.1).

mod elaborate;
mod interp;
mod value;

pub use elaborate::elaborate;
pub use interp::{eval_body, eval_body_with, EvalCtx, DEFAULT_CALL_DEPTH, DEFAULT_FUEL};
pub use value::{Env, EvalError, Value};

#[cfg(test)]
mod tests;
