//! Copyright (c) 2026 Omnira CJSC
//!
//! The Eidos whole-function constant-folding fast path.
//!
//! This is where the Eidos mid-level stack -- `codira_mir`'s IR and pass
//! pipeline, `codira_egraph`'s equality saturation, and
//! `codira_comptime`'s interpreter -- becomes load-bearing in the
//! *production* compile, rather than a facility reachable only from
//! tests.
//!
//! The pipeline per function:
//!
//! ```text
//! HIR body --(codira_hir::mir_lower)--> Eidos generator
//!          --(codira_mir::pass::full_pipeline)--> inlined/unrolled/cleaned
//!          --(codira_egraph::optimize)--> cost-minimal equivalent
//!          --(codira_comptime::eval_body)--> concrete value, if any
//!          --> `ret <typed constant>`
//! ```
//!
//! Analogous to what KGEN's elaborator achieves by interpreting generator
//! bodies at compile time (`modular/KGEN/lib/Elaborator`), except that the
//! search for the cheapest equivalent program runs first, so a body that
//! only *becomes* constant after inlining, unrolling, or reassociation is
//! still caught.
//!
//! # Why this is restricted to constants
//!
//! The obvious ambition would be to route *every* function body through
//! Eidos and lower the optimized MIR straight to LLVM (`mir_codegen` can
//! already do that). It would be **unsound today**, and the reason is
//! worth stating precisely because it is the single biggest remaining
//! limitation of the mid-level IR:
//!
//! **`codira_mir` is untyped.** `Attr::Int` is an `i64` with no width or
//! signedness, and `mir_codegen` consequently emits `i64` arithmetic. The
//! source language has `i8`..`i64`, `u8`..`u64`, `f32` and `f64`. Lowering
//! an `i32` function through MIR would emit 64-bit arithmetic and return
//! it from a function typed `i32` -- a miscompile, and one LLVM's verifier
//! would (rightly) reject.
//!
//! Folding to a *constant* sidesteps this entirely: the value is
//! materialized in the **HIR-derived LLVM type** ([`fold_to_constant`]
//! takes the type from the function signature, never from MIR), and is
//! range-checked against that type's width before being emitted. MIR's
//! `i64` assumption therefore cannot leak into the generated code -- at
//! worst the fold declines and the ordinary HIR->LLVM path runs.
//!
//! Giving `codira_mir` a real type system is the prerequisite for the
//! general path, and is tracked as the next step in
//! `spec/EIDOS_ARCHITECTURE.md`.

use codira_hir::HirDatabase;
use inkwell::values::{BasicValueEnum, FunctionValue};

/// Attempts to reduce `function`'s entire body to a compile-time constant
/// via the Eidos stack, returning it already typed for `fn_value`'s
/// return type.
///
/// `None` means "not foldable" -- never a wrong answer. Every step is
/// allowed to decline: the body may not lower to MIR at all, may still
/// reference runtime arguments, may fail to evaluate, or may produce a
/// value that does not fit the declared return type.
pub(crate) fn fold_to_constant<'ink>(
    db: &dyn HirDatabase,
    function: codira_hir::Function,
    fn_value: FunctionValue<'ink>,
) -> Option<BasicValueEnum<'ink>> {
    // A function with runtime parameters cannot be constant -- `core.arg`
    // would survive evaluation. (The interpreter would refuse anyway;
    // checking here avoids the work.)
    if !function.body(db).params().is_empty() {
        return None;
    }

    let generator = db.mir_generator(function)?;
    // A generator with unbound compile-time parameters has not been
    // specialized; it has no single value to fold to.
    if !generator.params.is_empty() {
        return None;
    }

    let mut body = generator.body.clone();

    // Interprocedural context for the `inline` pass: every other function
    // in this module, lowered to a generator. `mir_generator` is a salsa
    // query, so building this per function is a map lookup after the
    // first time each callee is lowered.
    let store = module_generator_store(db, function);

    // 1. Structural optimization: inline callees, unroll counted loops, propagate
    //    constants, canonicalize, CSE, then drop the corpses. Run to fixpoint so a
    //    constant exposed by inlining can feed the next round's unrolling.
    let pipeline = codira_mir::pass::full_pipeline();
    let ctx = codira_mir::pass::PassContext {
        generators: Some(&store),
    };
    if pipeline.run_to_fixpoint(&mut body, &ctx, 8).is_err() {
        // A pass produced invalid IR: a compiler bug, but not one this
        // function should paper over by emitting something. Decline and
        // let the ordinary path compile the function correctly.
        return None;
    }

    // 2. Equality saturation: explore every rewrite simultaneously and extract the
    //    cheapest form. Catches reassociations the ordered pipeline above cannot
    //    reach.
    let body = codira_egraph::optimize(&body);

    // 3. Evaluate. Anything referencing a runtime value, or exceeding the
    //    interpreter's fuel, declines here.
    let value = codira_comptime::eval_body(&body, &codira_comptime::Env::new()).ok()?;

    // 4. Materialize in the *declared* return type -- see the module doc on why the
    //    type must come from HIR and not from MIR.
    materialize(value, fn_value)
}

/// Collects every function in `function`'s module that lowers to an Eidos
/// generator, so `inline` can resolve `core.call` symbols.
///
/// Scope is deliberately one module: `OpKind::Call` carries a bare name,
/// and names are only unambiguous within a module. A call to another
/// module simply does not resolve, and the fold declines -- the same
/// honest-refusal rule the rest of this path follows.
///
/// `function` itself is included: it costs nothing (the `inline` pass
/// rejects a callee that still contains a call, so direct self-recursion
/// can never be inlined) and keeps the store independent of which
/// function is being compiled, which is what lets salsa share the
/// lowering work across the whole module.
fn module_generator_store(
    db: &dyn HirDatabase,
    function: codira_hir::Function,
) -> codira_mir::GeneratorStore {
    let mut store = codira_mir::GeneratorStore::new();
    for def in function.module(db).declarations(db) {
        if let codira_hir::ModuleDef::Function(f) = def {
            if let Some(generator) = db.mir_generator(f) {
                store.add_generator((*generator).clone());
            }
        }
    }
    store
}

/// Turns a comptime value into an LLVM constant of `fn_value`'s return
/// type, or `None` when the two do not correspond (including when the
/// value is out of range for the declared width).
fn materialize<'ink>(
    value: codira_comptime::Value,
    fn_value: FunctionValue<'ink>,
) -> Option<BasicValueEnum<'ink>> {
    let ret_ty = fn_value.get_type().get_return_type()?;
    match value {
        codira_comptime::Value::Int(v) => {
            let int_ty = ret_ty.into_int_type();
            let width = int_ty.get_bit_width();
            // A 1-bit return type is `bool`; an integer is not a bool.
            if width == 1 {
                return None;
            }
            if !fits_in_width(v, width) {
                // The comptime value does not fit the declared type. This
                // is a program error the type checker should report --
                // silently truncating it here would be the worst possible
                // response, so decline and let the normal path run.
                return None;
            }
            Some(int_ty.const_int(v as u64, true).into())
        }
        codira_comptime::Value::Bool(b) => {
            let int_ty = ret_ty.into_int_type();
            if int_ty.get_bit_width() != 1 {
                return None;
            }
            Some(int_ty.const_int(u64::from(b), false).into())
        }
        codira_comptime::Value::Float(f) => {
            if !ret_ty.is_float_type() {
                return None;
            }
            Some(ret_ty.into_float_type().const_float(f).into())
        }
        // Unit has no value representation here, and strings/tuples have
        // no constant ABI at this layer yet.
        codira_comptime::Value::Unit
        | codira_comptime::Value::Str(_)
        | codira_comptime::Value::Tuple(_) => None,
    }
}

/// Whether `v` is representable in `width` bits.
///
/// Checked against the *signed* range: `codira_comptime::Value::Int` is a
/// signed `i64`, and a negative comptime value returned from an unsigned
/// function is a type error rather than something to wrap silently. A
/// value in `0..2^width` that exceeds the signed maximum is still
/// accepted, so unsigned results near the top of their range fold
/// correctly.
fn fits_in_width(v: i64, width: u32) -> bool {
    if width >= 64 {
        return true;
    }
    let signed_min = -(1i64 << (width - 1));
    let unsigned_max = (1i64 << width) - 1;
    v >= signed_min && v <= unsigned_max
}

#[cfg(test)]
mod tests {
    use super::fits_in_width;

    #[test]
    fn width_bounds_accept_both_signednesses() {
        // i8 range
        assert!(fits_in_width(-128, 8));
        assert!(fits_in_width(127, 8));
        // u8 range above i8::MAX is still representable in 8 bits
        assert!(fits_in_width(255, 8));
        assert!(!fits_in_width(256, 8));
        assert!(!fits_in_width(-129, 8));
    }

    #[test]
    fn wide_types_accept_everything() {
        assert!(fits_in_width(i64::MIN, 64));
        assert!(fits_in_width(i64::MAX, 64));
    }

    #[test]
    fn thirty_two_bit_bounds() {
        assert!(fits_in_width(i64::from(i32::MAX), 32));
        assert!(fits_in_width(i64::from(u32::MAX), 32));
        assert!(!fits_in_width(i64::from(u32::MAX) + 1, 32));
        assert!(fits_in_width(i64::from(i32::MIN), 32));
        assert!(!fits_in_width(i64::from(i32::MIN) - 1, 32));
    }
}
