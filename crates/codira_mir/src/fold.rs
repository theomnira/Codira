//! Copyright (c) 2026 Omnira CJSC
//!
//! Attribute-level fold hooks: the one place that defines what each pure
//! op *means* on compile-time-known values.
//!
//! KGEN's comptime interpreter deliberately reuses MLIR fold hooks so that
//! constant folding, canonicalization, and comptime execution share a
//! single semantics implementation (`modular/KGEN/docs/overviews/
//! Interpreter.md`). This module is that idea for `codira_mir`: the
//! comptime interpreter (`codira_comptime`), the constant-folding pass
//! (`pass`), and the e-graph optimizer (`codira_egraph`) all call
//! [`fold_op`] rather than re-implementing arithmetic.
//!
//! Semantics reference (also documented on the op kinds themselves):
//! * `Int` is `i64` with wrapping add/sub/mul/neg (comptime integers are
//!   fixed-width two's complement, not bignums -- matching what codegen emits
//!   for the same ops).
//! * `Div`/`Rem` by zero and `i64::MIN / -1` overflow return a [`FoldError`],
//!   never a wrong value or a panic.
//! * Shifts with a count outside `0..64` are errors (LLVM would call this
//!   poison; the comptime layer refuses instead).
//! * `And`/`Or` here are the *non*-short-circuiting bool ops (both operands
//!   already evaluated). Short-circuiting is a control-flow concern and belongs
//!   to the interpreter's `If` handling.
//! * Comparisons and equality work on any two attrs of the same shape; mixed
//!   shapes are a type error.
//! * Float arithmetic is IEEE `f64`; `Eq`/`Ne` on floats are IEEE (so `NaN !=
//!   NaN`), unlike `Attr`'s own bitwise identity.

use crate::{
    op::{Attr, CastKind, OpKind},
    ty::TypeId,
};

/// Everything that can go wrong folding one op over concrete attributes.
/// Mirrored (and wrapped) by `codira_comptime::EvalError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    #[error("type mismatch: expected {expected}, found {found}")]
    TypeMismatch {
        expected: &'static str,
        found: &'static str,
    },
    #[error("division by zero")]
    DivideByZero,
    #[error("integer overflow in division")]
    DivisionOverflow,
    #[error("shift amount out of range 0..64")]
    ShiftOutOfRange,
    #[error("wrong operand count")]
    WrongOperandCount,
    #[error("op is not foldable at the attribute level")]
    NotFoldable,
}

fn type_name(attr: &Attr) -> &'static str {
    match attr {
        Attr::Int(_) => "int",
        Attr::Bool(_) => "bool",
        Attr::Float(_) => "float",
        Attr::Str(_) => "string",
        Attr::Unit => "unit",
        Attr::ParamRef(_) => "unresolved parameter",
    }
}

fn as_int(attr: &Attr) -> Result<i64, FoldError> {
    match attr {
        Attr::Int(v) => Ok(*v),
        other => Err(FoldError::TypeMismatch {
            expected: "int",
            found: type_name(other),
        }),
    }
}

fn as_bool(attr: &Attr) -> Result<bool, FoldError> {
    match attr {
        Attr::Bool(v) => Ok(*v),
        other => Err(FoldError::TypeMismatch {
            expected: "bool",
            found: type_name(other),
        }),
    }
}

/// Folds one pure op applied to fully-concrete operand attributes.
///
/// Returns `Err(FoldError::NotFoldable)` for op kinds that are not pure
/// attribute-level computations (control flow, calls, references) --
/// callers treat that as "leave the op alone", distinct from a genuine
/// evaluation error like division by zero.
// `Cast` and the control-flow catch-all both yield `NotFoldable`, but for
// different reasons the comments below record; merging the arms to satisfy
// clippy::match_same_arms would delete that distinction.
#[allow(clippy::match_same_arms)]
pub fn fold_op(kind: &OpKind, operands: &[Attr]) -> Result<Attr, FoldError> {
    use OpKind::{
        Add, And, Arg, BitAnd, BitNot, BitOr, BitXor, BlockArg, Call, Cast, Const, Div, Eq, For,
        Ge, Gt, If, Le, Lt, Mul, Ne, Neg, Not, Or, ParamRef, Rem, Shl, Shr, Sub, Tuple, TupleGet,
        While, Yield,
    };

    let unary = || -> Result<&Attr, FoldError> {
        match operands {
            [a] => Ok(a),
            _ => Err(FoldError::WrongOperandCount),
        }
    };
    let binary = || -> Result<(&Attr, &Attr), FoldError> {
        match operands {
            [a, b] => Ok((a, b)),
            _ => Err(FoldError::WrongOperandCount),
        }
    };

    match kind {
        Const(attr) => {
            if operands.is_empty() {
                Ok(attr.clone())
            } else {
                Err(FoldError::WrongOperandCount)
            }
        }

        Add | Sub | Mul => {
            let (a, b) = binary()?;
            match (a, b) {
                (Attr::Float(_), _) | (_, Attr::Float(_)) => {
                    let (x, y) = (float_operand(a)?, float_operand(b)?);
                    Ok(Attr::float(match kind {
                        Add => x + y,
                        Sub => x - y,
                        _ => x * y,
                    }))
                }
                _ => {
                    let (x, y) = (as_int(a)?, as_int(b)?);
                    Ok(Attr::Int(match kind {
                        Add => x.wrapping_add(y),
                        Sub => x.wrapping_sub(y),
                        _ => x.wrapping_mul(y),
                    }))
                }
            }
        }

        Div | Rem => {
            let (a, b) = binary()?;
            match (a, b) {
                (Attr::Float(_), _) | (_, Attr::Float(_)) => {
                    let (x, y) = (float_operand(a)?, float_operand(b)?);
                    // IEEE division by zero is inf/NaN, not an error.
                    Ok(Attr::float(if matches!(kind, Div) { x / y } else { x % y }))
                }
                _ => {
                    let (x, y) = (as_int(a)?, as_int(b)?);
                    if y == 0 {
                        return Err(FoldError::DivideByZero);
                    }
                    if x == i64::MIN && y == -1 {
                        return Err(FoldError::DivisionOverflow);
                    }
                    Ok(Attr::Int(if matches!(kind, Div) { x / y } else { x % y }))
                }
            }
        }

        Neg => match unary()? {
            Attr::Int(v) => Ok(Attr::Int(v.wrapping_neg())),
            Attr::Float(bits) => Ok(Attr::float(-f64::from_bits(*bits))),
            other => Err(FoldError::TypeMismatch {
                expected: "int or float",
                found: type_name(other),
            }),
        },

        Eq | Ne => {
            let (a, b) = binary()?;
            let equal = match (a, b) {
                // IEEE equality for floats (NaN != NaN) -- see module doc.
                (Attr::Float(_), _) | (_, Attr::Float(_)) => float_operand(a)? == float_operand(b)?,
                (Attr::Int(x), Attr::Int(y)) => x == y,
                (Attr::Bool(x), Attr::Bool(y)) => x == y,
                (Attr::Str(x), Attr::Str(y)) => x == y,
                (Attr::Unit, Attr::Unit) => true,
                _ => {
                    return Err(FoldError::TypeMismatch {
                        expected: "operands of the same type",
                        found: "mixed types",
                    })
                }
            };
            Ok(Attr::Bool(if matches!(kind, Eq) { equal } else { !equal }))
        }

        Lt | Le | Gt | Ge => {
            let (a, b) = binary()?;
            let ordering_holds = match (a, b) {
                (Attr::Float(_), _) | (_, Attr::Float(_)) => {
                    let (x, y) = (float_operand(a)?, float_operand(b)?);
                    match kind {
                        Lt => x < y,
                        Le => x <= y,
                        Gt => x > y,
                        _ => x >= y,
                    }
                }
                _ => {
                    let (x, y) = (as_int(a)?, as_int(b)?);
                    match kind {
                        Lt => x < y,
                        Le => x <= y,
                        Gt => x > y,
                        _ => x >= y,
                    }
                }
            };
            Ok(Attr::Bool(ordering_holds))
        }

        And | Or => {
            let (a, b) = binary()?;
            let (x, y) = (as_bool(a)?, as_bool(b)?);
            Ok(Attr::Bool(if matches!(kind, And) {
                x && y
            } else {
                x || y
            }))
        }

        Not => Ok(Attr::Bool(!as_bool(unary()?)?)),

        // Bitwise complement. `!v` on `i64` is the two's-complement
        // complement; callers that need a narrower width re-truncate via
        // a cast, exactly as the source language does.
        BitNot => Ok(Attr::Int(!as_int(unary()?)?)),

        BitAnd | BitOr | BitXor => {
            let (a, b) = binary()?;
            let (x, y) = (as_int(a)?, as_int(b)?);
            Ok(Attr::Int(match kind {
                BitAnd => x & y,
                BitOr => x | y,
                _ => x ^ y,
            }))
        }

        Shl | Shr => {
            let (a, b) = binary()?;
            let (x, y) = (as_int(a)?, as_int(b)?);
            // The conversion can only fail because `y` is negative or
            // exceeds u32, both of which are exactly "shift out of
            // range"; the TryFromIntError carries nothing further.
            let Ok(amount) = u32::try_from(y) else {
                return Err(FoldError::ShiftOutOfRange);
            };
            if amount >= 64 {
                return Err(FoldError::ShiftOutOfRange);
            }
            Ok(Attr::Int(if matches!(kind, Shl) {
                x.wrapping_shl(amount)
            } else {
                x >> amount // arithmetic shift: `Attr::Int` is signed
            }))
        }

        // A cast's result depends on its *target type*, which lives on
        // the `Op` (`Op::ty`), not in the `OpKind` -- so it cannot be
        // folded through this signature. Folding casts is unlocked when
        // `fold_op` becomes type-directed in RFC-001 SS1.8 phase 3; until
        // then a constant cast survives to codegen, which is correct if
        // suboptimal.
        Cast(..) => Err(FoldError::NotFoldable),

        // Everything else is control flow, aggregation, or a reference --
        // not an attribute-level computation.
        Tuple | TupleGet(_) | Call(_) | ParamRef(_) | Arg(_) | BlockArg(_) | If | While | For
        | Yield => Err(FoldError::NotFoldable),
    }
}

fn float_operand(attr: &Attr) -> Result<f64, FoldError> {
    match attr {
        Attr::Float(bits) => Ok(f64::from_bits(*bits)),
        // Int-to-float promotion in mixed arithmetic: comptime literals
        // like `2 * 3.14` should fold without requiring the frontend to
        // insert casts the language doesn't have yet.
        Attr::Int(v) => Ok(*v as f64),
        other => Err(FoldError::TypeMismatch {
            expected: "float",
            found: type_name(other),
        }),
    }
}

/// Folds a conversion on a compile-time-known value.
///
/// Kept separate from [`fold_op`] because a cast's result depends on its
/// **target type**, which lives on the `Op` rather than in the `OpKind` --
/// `fold_op`'s `(kind, operands)` signature simply cannot express it. The
/// source type is required too: `zext` and `sext` differ only in how the
/// *source* width's top bit is treated, and `Attr::Int` is a bare `i64`
/// that does not record which width it came from.
///
/// Semantics follow `spec/LANGUAGE_SPEC.md` section 18 exactly:
/// integer conversions wrap, and float-to-integer conversions **saturate**
/// with `NaN` mapping to zero.
///
/// `Err(FoldError::NotFoldable)` means "leave the op alone" (e.g. the
/// operand is not a constant of the right shape), never a wrong answer.
pub fn fold_cast(
    kind: CastKind,
    value: &Attr,
    source: TypeId,
    target: TypeId,
) -> Result<Attr, FoldError> {
    match kind {
        CastKind::Trunc | CastKind::Zext | CastKind::Sext | CastKind::Bitcast => {
            let v = match value {
                Attr::Int(v) => *v,
                // `bool -> int` is a zero-extension in the spec matrix.
                Attr::Bool(b) => i64::from(*b),
                _ => return Err(FoldError::NotFoldable),
            };
            let (target_width, target_signed) = target.as_int().ok_or(FoldError::NotFoldable)?;
            match kind {
                // Truncation and bitcast both keep the low bits of the
                // target width; the result's interpretation comes from
                // the target's signedness.
                CastKind::Trunc | CastKind::Bitcast => {
                    Ok(Attr::Int(reinterpret(v, target_width, target_signed)))
                }
                // Extension reads the low bits of the *source* width and
                // fills according to the direction being requested.
                CastKind::Zext => {
                    let source_width = source.as_int().map_or(1, |(w, _)| w);
                    Ok(Attr::Int(reinterpret(v, source_width, false)))
                }
                CastKind::Sext => {
                    let source_width = source.as_int().map_or(1, |(w, _)| w);
                    Ok(Attr::Int(reinterpret(v, source_width, true)))
                }
                _ => unreachable!("outer match restricts the kinds"),
            }
        }

        CastKind::FpTrunc | CastKind::FpExt => {
            let v = value.as_float().ok_or(FoldError::NotFoldable)?;
            match target.as_float() {
                // Narrowing to f32 must actually round through f32, or
                // the folded constant would carry precision the runtime
                // value cannot have.
                Some(32) => Ok(Attr::float(f64::from(v as f32))),
                Some(64) => Ok(Attr::float(v)),
                _ => Err(FoldError::NotFoldable),
            }
        }

        CastKind::SiToFp | CastKind::UiToFp => {
            let v = match value {
                Attr::Int(v) => *v,
                Attr::Bool(b) => i64::from(*b),
                _ => return Err(FoldError::NotFoldable),
            };
            let as_f64 = if matches!(kind, CastKind::SiToFp) {
                v as f64
            } else {
                // Unsigned: reinterpret the source width's bits first, so
                // a u64 above i64::MAX converts to the right magnitude.
                let source_width = source.as_int().map_or(64, |(w, _)| w);
                let unsigned = reinterpret(v, source_width, false);
                if source_width >= 64 {
                    (unsigned as u64) as f64
                } else {
                    unsigned as f64
                }
            };
            match target.as_float() {
                Some(32) => Ok(Attr::float(f64::from(as_f64 as f32))),
                Some(64) => Ok(Attr::float(as_f64)),
                _ => Err(FoldError::NotFoldable),
            }
        }

        CastKind::FpToSi | CastKind::FpToUi => {
            let v = value.as_float().ok_or(FoldError::NotFoldable)?;
            let (width, _) = target.as_int().ok_or(FoldError::NotFoldable)?;
            let signed = matches!(kind, CastKind::FpToSi);
            Ok(Attr::Int(saturate_to_int(v, width, signed)))
        }
    }
}

/// Reinterprets the low `width` bits of `v` as a signed or unsigned value
/// of that width, returned in `i64`'s domain.
fn reinterpret(v: i64, width: u16, signed: bool) -> i64 {
    if width >= 64 {
        return v;
    }
    let shift = 64 - u32::from(width);
    if signed {
        // Shift up then arithmetic-shift down: sign-extends from `width`.
        (v << shift) >> shift
    } else {
        // Mask to `width` bits, leaving a non-negative value.
        let mask = (1i64 << width) - 1;
        v & mask
    }
}

/// Saturating float-to-integer conversion, matching
/// `spec/LANGUAGE_SPEC.md` section 18.3 and LLVM's `llvm.fpto{s,u}i.sat`:
/// out-of-range clamps to the destination bounds and `NaN` maps to zero.
fn saturate_to_int(v: f64, width: u16, signed: bool) -> i64 {
    if v.is_nan() {
        return 0;
    }
    let (min, max) = if signed {
        if width >= 64 {
            (i64::MIN, i64::MAX)
        } else {
            (-(1i64 << (width - 1)), (1i64 << (width - 1)) - 1)
        }
    } else if width >= 64 {
        // u64's upper half does not fit i64; clamp to what `Attr::Int`
        // can represent rather than wrapping into a negative number.
        (0, i64::MAX)
    } else {
        (0, (1i64 << width) - 1)
    };
    let truncated = v.trunc();
    if truncated <= min as f64 {
        min
    } else if truncated >= max as f64 {
        max
    } else {
        truncated as i64
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn int(v: i64) -> Attr {
        Attr::Int(v)
    }

    #[test]
    fn arithmetic_wraps() {
        assert_eq!(
            fold_op(&OpKind::Add, &[int(i64::MAX), int(1)]),
            Ok(int(i64::MIN))
        );
        assert_eq!(fold_op(&OpKind::Neg, &[int(i64::MIN)]), Ok(int(i64::MIN)));
    }

    #[test]
    fn division_guards() {
        assert_eq!(
            fold_op(&OpKind::Div, &[int(1), int(0)]),
            Err(FoldError::DivideByZero)
        );
        assert_eq!(
            fold_op(&OpKind::Div, &[int(i64::MIN), int(-1)]),
            Err(FoldError::DivisionOverflow)
        );
        assert_eq!(fold_op(&OpKind::Rem, &[int(7), int(3)]), Ok(int(1)));
    }

    #[test]
    fn shifts_are_checked() {
        assert_eq!(fold_op(&OpKind::Shl, &[int(1), int(3)]), Ok(int(8)));
        assert_eq!(
            fold_op(&OpKind::Shl, &[int(1), int(64)]),
            Err(FoldError::ShiftOutOfRange)
        );
        assert_eq!(
            fold_op(&OpKind::Shr, &[int(-8), int(1)]),
            Ok(int(-4)),
            "right shift is arithmetic"
        );
    }

    #[test]
    fn float_semantics() {
        assert_eq!(
            fold_op(&OpKind::Add, &[Attr::float(1.5), Attr::float(2.25)]),
            Ok(Attr::float(3.75))
        );
        // Mixed int/float promotes.
        assert_eq!(
            fold_op(&OpKind::Mul, &[int(2), Attr::float(3.5)]),
            Ok(Attr::float(7.0))
        );
        // IEEE: NaN != NaN even though the attrs are bit-identical.
        let nan = Attr::float(f64::NAN);
        assert_eq!(
            fold_op(&OpKind::Eq, &[nan.clone(), nan]),
            Ok(Attr::Bool(false))
        );
    }

    #[test]
    fn control_flow_is_not_foldable() {
        assert_eq!(fold_op(&OpKind::If, &[]), Err(FoldError::NotFoldable));
        assert_eq!(
            fold_op(&OpKind::Call("f".into()), &[]),
            Err(FoldError::NotFoldable)
        );
    }

    #[test]
    fn cast_truncation_wraps() {
        // 300 as u8 == 44 (300 mod 256) -- the spec's worked example.
        assert_eq!(
            fold_cast(CastKind::Trunc, &int(300), TypeId::I64, TypeId::U8),
            Ok(int(44))
        );
        // -1 as u8 == 255
        assert_eq!(
            fold_cast(CastKind::Trunc, &int(-1), TypeId::I64, TypeId::U8),
            Ok(int(255))
        );
        // -1 as i8 == -1
        assert_eq!(
            fold_cast(CastKind::Trunc, &int(-1), TypeId::I64, TypeId::I8),
            Ok(int(-1))
        );
    }

    #[test]
    fn extension_direction_comes_from_the_source() {
        // The classic cast bug: u32 0xFFFF_FFFF widened to i64 must be
        // 4294967295, NOT -1. Zext and sext differ *only* here.
        assert_eq!(
            fold_cast(CastKind::Zext, &int(-1), TypeId::U32, TypeId::I64),
            Ok(int(4294967295))
        );
        assert_eq!(
            fold_cast(CastKind::Sext, &int(-1), TypeId::I32, TypeId::I64),
            Ok(int(-1))
        );
    }

    #[test]
    fn float_to_int_saturates_and_maps_nan_to_zero() {
        assert_eq!(
            fold_cast(
                CastKind::FpToSi,
                &Attr::float(1e20),
                TypeId::F64,
                TypeId::I32
            ),
            Ok(int(i64::from(i32::MAX)))
        );
        assert_eq!(
            fold_cast(
                CastKind::FpToSi,
                &Attr::float(-1e20),
                TypeId::F64,
                TypeId::I32
            ),
            Ok(int(i64::from(i32::MIN)))
        );
        assert_eq!(
            fold_cast(
                CastKind::FpToSi,
                &Attr::float(f64::NAN),
                TypeId::F64,
                TypeId::I32
            ),
            Ok(int(0))
        );
        // Ordinary in-range values truncate toward zero.
        assert_eq!(
            fold_cast(
                CastKind::FpToSi,
                &Attr::float(3.7),
                TypeId::F64,
                TypeId::I32
            ),
            Ok(int(3))
        );
        assert_eq!(
            fold_cast(
                CastKind::FpToSi,
                &Attr::float(-3.7),
                TypeId::F64,
                TypeId::I32
            ),
            Ok(int(-3))
        );
    }

    #[test]
    fn float_narrowing_rounds_through_f32() {
        // A constant that is exact in f64 but not f32 must lose the same
        // precision the runtime conversion would.
        let v = 0.1f64;
        let folded = fold_cast(CastKind::FpTrunc, &Attr::float(v), TypeId::F64, TypeId::F32);
        assert_eq!(folded, Ok(Attr::float(f64::from(v as f32))));
        assert_ne!(folded, Ok(Attr::float(v)), "f32 rounding must be applied");
    }

    #[test]
    fn int_to_float_respects_source_signedness() {
        // u64 bit pattern -1 is 18446744073709551615, not -1.0.
        let unsigned = fold_cast(CastKind::UiToFp, &int(-1), TypeId::U64, TypeId::F64);
        assert_eq!(unsigned, Ok(Attr::float(u64::MAX as f64)));
        let signed = fold_cast(CastKind::SiToFp, &int(-1), TypeId::I64, TypeId::F64);
        assert_eq!(signed, Ok(Attr::float(-1.0)));
    }

    #[test]
    fn non_constant_shapes_decline() {
        assert_eq!(
            fold_cast(CastKind::Trunc, &Attr::Unit, TypeId::I64, TypeId::I8),
            Err(FoldError::NotFoldable)
        );
        assert_eq!(
            fold_cast(CastKind::FpToSi, &int(1), TypeId::I64, TypeId::I32),
            Err(FoldError::NotFoldable),
            "a float conversion needs a float operand"
        );
    }
}
