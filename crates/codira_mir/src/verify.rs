//! Copyright (c) 2026 Omnira CJSC
//!
//! Structural verifier for [`Body`]: the machine-checked version of every
//! arity/region contract written in `op.rs`'s doc comments. The analog of
//! KGEN's `kgen-verifier` pass (`modular/KGEN/include/KGEN/KGENPasses.td`),
//! which runs after every major pipeline phase there; passes here are
//! expected to call [`verify_body`] in tests (and debug builds) the same
//! way.
//!
//! SSA dominance needs no checking: `la_arena` ids are allocation-ordered
//! and `Body::push` collects operands before allocating, so an operand can
//! only name an earlier op. What *can* go wrong -- and is checked -- is
//! arity, region count/shape, `Yield` placement, and `BlockArg` indices.

use crate::op::{Body, Op, OpId, OpKind, Region};

/// A structural rule violation, with the offending op's id (within its own
/// region's body -- ids are region-local, matching everything else in this
/// crate) and a static description of the broken rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("verifier: op {op:?}: {rule}")]
pub struct VerifyError {
    pub op: OpId,
    pub rule: &'static str,
}

/// Verifies `body` as a *top-level* region body: no enclosing region, so
/// `BlockArg` is not valid at this level (function runtime inputs are
/// `Arg`, not `BlockArg`). Recurses into all nested regions with the
/// correct argument counts.
pub fn verify_body(body: &Body) -> Result<(), VerifyError> {
    verify_in_region(body, 0, false)
}

fn verify_in_region(body: &Body, num_args: u32, yield_expected: bool) -> Result<(), VerifyError> {
    let last_id = body.result();
    for (id, op) in body.iter() {
        verify_op(id, op, num_args)?;

        // `Yield` is only legal as the final op of a loop-body region.
        if matches!(op.kind, OpKind::Yield) && (!yield_expected || Some(id) != last_id) {
            return Err(VerifyError {
                op: id,
                rule: "cf.yield is only valid as the last op of a loop body region",
            });
        }
    }
    if yield_expected {
        let yields = last_id.map(|id| matches!(body.get(id).kind, OpKind::Yield));
        if yields != Some(true) {
            return Err(VerifyError {
                op: last_id.unwrap_or_else(|| {
                    // An empty body that was supposed to yield: report op 0
                    // of an empty arena is impossible, so synthesize via a
                    // throwaway arena is not worth it -- reuse last_id
                    // unreachable path by constructing from a fresh body.
                    // In practice `Body::result()` is `None` only here.
                    let mut probe = Body::new();
                    probe.push(OpKind::Const(crate::Attr::Unit), [])
                }),
                rule: "loop body region must end in cf.yield",
            });
        }
    }
    Ok(())
}

fn verify_op(id: OpId, op: &Op, num_args: u32) -> Result<(), VerifyError> {
    use OpKind::{
        Add, And, Arg, BitAnd, BitNot, BitOr, BitXor, BlockArg, Call, Cast, Const, Div, Eq, For,
        Ge, Gt, If, Le, Lt, Mul, Ne, Neg, Not, Or, ParamRef, Rem, Shl, Shr, Sub, Tuple, TupleGet,
        While, Yield,
    };

    let fail = |rule: &'static str| Err(VerifyError { op: id, rule });

    let expect_operands = |n: usize, rule: &'static str| {
        if op.operands.len() == n {
            Ok(())
        } else {
            Err(VerifyError { op: id, rule })
        }
    };
    let expect_no_regions = |rule: &'static str| {
        if op.regions.is_empty() {
            Ok(())
        } else {
            Err(VerifyError { op: id, rule })
        }
    };

    match &op.kind {
        Const(_) => {
            expect_operands(0, "core.const takes no operands")?;
            expect_no_regions("core.const takes no regions")
        }
        ParamRef(_) => {
            expect_operands(0, "param.ref takes no operands")?;
            expect_no_regions("param.ref takes no regions")
        }
        Arg(_) => {
            expect_operands(0, "core.arg takes no operands")?;
            expect_no_regions("core.arg takes no regions")
        }
        BlockArg(i) => {
            expect_operands(0, "cf.block_arg takes no operands")?;
            expect_no_regions("cf.block_arg takes no regions")?;
            if *i >= num_args {
                return fail("cf.block_arg index out of range for enclosing region");
            }
            Ok(())
        }

        Neg | Not | BitNot | TupleGet(_) => {
            expect_operands(1, "unary op takes exactly one operand")?;
            expect_no_regions("unary op takes no regions")
        }

        Cast(_, _) => {
            expect_operands(1, "core.cast takes exactly one operand")?;
            expect_no_regions("core.cast takes no regions")
        }

        Add | Sub | Mul | Div | Rem | Eq | Ne | Lt | Le | Gt | Ge | And | Or | BitAnd | BitOr
        | BitXor | Shl | Shr => {
            expect_operands(2, "binary op takes exactly two operands")?;
            expect_no_regions("binary op takes no regions")
        }

        Tuple | Call(_) | Yield => expect_no_regions("op takes no regions"),

        If => {
            expect_operands(1, "cf.if takes exactly one operand (the condition)")?;
            let [then_r, else_r] = op.regions.as_slice() else {
                return fail("cf.if takes exactly two regions (then, else)");
            };
            for r in [then_r, else_r] {
                if r.num_args != 0 {
                    return fail("cf.if regions take no block arguments");
                }
                verify_in_region(&r.body, num_args, false)?;
            }
            Ok(())
        }

        While => {
            let n = op.operands.len() as u32;
            let [cond_r, body_r] = op.regions.as_slice() else {
                return fail("cf.while takes exactly two regions (cond, body)");
            };
            if cond_r.num_args != n || body_r.num_args != n {
                return fail("cf.while regions must take one block arg per carried value");
            }
            if cond_r.body.is_empty() {
                return fail("cf.while cond region must not be empty");
            }
            verify_in_region(&cond_r.body, n, false)?;
            verify_in_region(&body_r.body, n, true)?;
            verify_yield_arity(&body_r.body, n as usize, id)
        }

        For => {
            if op.operands.len() < 3 {
                return fail("cf.for takes at least three operands (start, end, step)");
            }
            let n = (op.operands.len() - 3) as u32;
            let [body_r] = op.regions.as_slice() else {
                return fail("cf.for takes exactly one region (body)");
            };
            if body_r.num_args != n + 1 {
                return fail("cf.for body must take the induction variable plus one block arg per carried value");
            }
            verify_in_region(&body_r.body, n + 1, true)?;
            verify_yield_arity(&body_r.body, n as usize, id)
        }
    }
}

fn verify_yield_arity(body: &Body, expected: usize, loop_op: OpId) -> Result<(), VerifyError> {
    let Some(last) = body.result() else {
        return Err(VerifyError {
            op: loop_op,
            rule: "loop body region must end in cf.yield",
        });
    };
    let last_op = body.get(last);
    if matches!(last_op.kind, OpKind::Yield) && last_op.operands.len() != expected {
        return Err(VerifyError {
            op: loop_op,
            rule: "cf.yield must name exactly one value per loop-carried value",
        });
    }
    Ok(())
}

/// Convenience: verify a region directly (used by passes that rewrite one
/// nested region at a time).
pub fn verify_region(region: &Region, yield_expected: bool) -> Result<(), VerifyError> {
    verify_in_region(&region.body, region.num_args, yield_expected)
}

// ---------------------------------------------------------------------------
// Type checking (RFC-001 section 1.4)
// ---------------------------------------------------------------------------

use crate::{op::CastKind, ty::TypeId};

/// A type rule violation: the op's declared `Op::ty` disagrees with what
/// its kind and operand types imply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("type error: op {op:?}: {rule}")]
pub struct TypeError {
    pub op: OpId,
    pub rule: &'static str,
}

/// Checks the typing rules over `body`.
///
/// Ops carrying `TypeId::UNTYPED` are **skipped**, not rejected: during
/// the RFC-001 phase-1/2 migration most ops are not yet typed, and a
/// partially-typed body must still verify. When an op *is* typed, its
/// operands are checked against it -- so checking strengthens
/// automatically as `mir_lower` is migrated, with no flag day.
///
/// This is the payoff for storing `ty` redundantly on every op: a rewrite
/// that produces a type-inconsistent body fails *here*, naming the op,
/// rather than surviving to LLVM's verifier.
pub fn check_types(body: &Body) -> Result<(), TypeError> {
    for (id, op) in body.iter() {
        check_op_types(body, id, op)?;
        for region in &op.regions {
            check_types(&region.body)?;
        }
    }
    Ok(())
}

/// The declared type of an operand, or `UNTYPED` when absent.
fn operand_ty(body: &Body, op: &Op, n: usize) -> TypeId {
    op.operands
        .get(n)
        .map_or(TypeId::UNTYPED, |&id| body.get(id).ty)
}

fn check_op_types(body: &Body, id: OpId, op: &Op) -> Result<(), TypeError> {
    use OpKind::{
        Add, And, Arg, BitAnd, BitNot, BitOr, BitXor, BlockArg, Call, Cast, Const, Div, Eq, For,
        Ge, Gt, If, Le, Lt, Mul, Ne, Neg, Not, Or, ParamRef, Rem, Shl, Shr, Sub, Tuple, TupleGet,
        While, Yield,
    };

    let fail = |rule: &'static str| Err(TypeError { op: id, rule });
    let ty = op.ty;

    match &op.kind {
        // Binary arithmetic: operands and result share one type, which
        // must be arithmetic. Mixed int/float is rejected -- promotion is
        // the frontend's job, expressed as an explicit cast.
        Add | Sub | Mul | Div | Rem => {
            if ty.is_untyped() {
                return Ok(());
            }
            if !ty.is_arithmetic() {
                return fail("arithmetic op result must be an integer or float type");
            }
            for n in 0..2 {
                let operand = operand_ty(body, op, n);
                if !operand.is_untyped() && operand != ty {
                    return fail("arithmetic operands must have the same type as the result");
                }
            }
            Ok(())
        }

        // Bitwise and shifts are integer-only, same type throughout
        // (matching LLVM's same-type shift requirement).
        BitAnd | BitOr | BitXor | Shl | Shr => {
            if ty.is_untyped() {
                return Ok(());
            }
            if !ty.is_int() {
                return fail("bitwise/shift op requires an integer type");
            }
            for n in 0..2 {
                let operand = operand_ty(body, op, n);
                if !operand.is_untyped() && operand != ty {
                    return fail("bitwise/shift operands must have the same type as the result");
                }
            }
            Ok(())
        }

        BitNot => {
            if ty.is_untyped() {
                return Ok(());
            }
            if !ty.is_int() {
                return fail("core.bitnot requires an integer type");
            }
            let x = operand_ty(body, op, 0);
            if !x.is_untyped() && x != ty {
                return fail("core.bitnot operand must have the same type as the result");
            }
            Ok(())
        }

        Neg => {
            if ty.is_untyped() {
                return Ok(());
            }
            if !ty.is_arithmetic() {
                return fail("core.neg requires an integer or float type");
            }
            let x = operand_ty(body, op, 0);
            if !x.is_untyped() && x != ty {
                return fail("core.neg operand must have the same type as the result");
            }
            Ok(())
        }

        // Comparisons: operands agree with each other; result is bool.
        Eq | Ne | Lt | Le | Gt | Ge => {
            if !ty.is_untyped() && ty != TypeId::BOOL {
                return fail("comparison result must be bool");
            }
            let (l, r) = (operand_ty(body, op, 0), operand_ty(body, op, 1));
            if !l.is_untyped() && !r.is_untyped() && l != r {
                return fail("comparison operands must have the same type");
            }
            Ok(())
        }

        And | Or | Not => {
            if !ty.is_untyped() && ty != TypeId::BOOL {
                return fail("logical op result must be bool");
            }
            for n in 0..op.operands.len() {
                let operand = operand_ty(body, op, n);
                if !operand.is_untyped() && operand != TypeId::BOOL {
                    return fail("logical op operands must be bool");
                }
            }
            Ok(())
        }

        // Conversions: category and width legality.
        Cast(kind, _) => {
            let src = operand_ty(body, op, 0);
            if ty.is_untyped() || src.is_untyped() {
                return Ok(());
            }
            match kind {
                CastKind::Bitcast => match (src.scalar_width(), ty.scalar_width()) {
                    (Some(a), Some(b)) if a == b => Ok(()),
                    (Some(_), Some(_)) => {
                        fail("core.bitcast operand and result must have equal width")
                    }
                    _ => fail("core.bitcast requires scalar operand and result types"),
                },
                CastKind::Trunc => match (src.as_int(), ty.as_int()) {
                    (Some((from, _)), Some((to, _))) if to < from => Ok(()),
                    (Some(_), Some(_)) => fail("core.trunc must narrow"),
                    _ => fail("core.trunc requires integer operand and result"),
                },
                CastKind::Zext | CastKind::Sext => match (src.as_int(), ty.as_int()) {
                    (Some((from, _)), Some((to, _))) if to > from => Ok(()),
                    (Some(_), Some(_)) => fail("integer extension must widen"),
                    _ => fail("integer extension requires integer operand and result"),
                },
                CastKind::FpTrunc => match (src.as_float(), ty.as_float()) {
                    (Some(from), Some(to)) if to < from => Ok(()),
                    (Some(_), Some(_)) => fail("core.fptrunc must narrow"),
                    _ => fail("core.fptrunc requires float operand and result"),
                },
                CastKind::FpExt => match (src.as_float(), ty.as_float()) {
                    (Some(from), Some(to)) if to > from => Ok(()),
                    (Some(_), Some(_)) => fail("core.fpext must widen"),
                    _ => fail("core.fpext requires float operand and result"),
                },
                CastKind::SiToFp | CastKind::UiToFp => {
                    if src.is_int() && ty.is_float() {
                        Ok(())
                    } else {
                        fail("int-to-float cast requires integer operand and float result")
                    }
                }
                CastKind::FpToSi | CastKind::FpToUi => {
                    if src.is_float() && ty.is_int() {
                        Ok(())
                    } else {
                        fail("float-to-int cast requires float operand and integer result")
                    }
                }
            }
        }

        If => {
            let cond = operand_ty(body, op, 0);
            if !cond.is_untyped() && cond != TypeId::BOOL {
                return fail("cf.if condition must be bool");
            }
            if ty.is_untyped() {
                return Ok(());
            }
            for region in &op.regions {
                if let Some(result) = region.body.result() {
                    let branch = region.body.get(result).ty;
                    if !branch.is_untyped() && branch != ty {
                        return fail("cf.if branches must have the same type as the result");
                    }
                }
            }
            Ok(())
        }

        // Loop-carried values must agree between the initial operands and
        // the values cf.yield feeds back. This is the check that catches
        // yielding a float into an int-initialized carried value --
        // previously invisible until LLVM's verifier.
        While | For => {
            let skip = if matches!(op.kind, For) { 3 } else { 0 };
            let Some(body_region) = op.regions.last() else {
                return Ok(());
            };
            let Some(last) = body_region.body.result() else {
                return Ok(());
            };
            let yield_op = body_region.body.get(last);
            if !matches!(yield_op.kind, Yield) {
                return Ok(());
            }
            for (n, &yielded) in yield_op.operands.iter().enumerate() {
                let init = operand_ty(body, op, skip + n);
                let next = body_region.body.get(yielded).ty;
                if !init.is_untyped() && !next.is_untyped() && init != next {
                    return fail(
                        "loop-carried value type must match between the initial value and cf.yield",
                    );
                }
            }
            Ok(())
        }

        // Leaves and aggregates carry whatever type they were assigned;
        // there is nothing local to check them against.
        Const(_) | Arg(_) | BlockArg(_) | ParamRef(_) | Tuple | TupleGet(_) | Call(_) | Yield => {
            Ok(())
        }
    }
}
