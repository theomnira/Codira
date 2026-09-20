//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! The legality matrix for `expr as Type` (see `spec/LANGUAGE_SPEC.md`), plus
//! the selection of the machine operation each legal cast performs.
//!
//! This lives in one place on purpose: type inference uses it to reject
//! illegal casts (see [`crate::ty::infer`]) and MIR lowering uses it to pick
//! the [`CastKind`] to emit, so the two can never disagree about what `as`
//! means.
//!
//! The rules, in full:
//!
//! * **Int -> Int** is *wrapping*: two's-complement truncation when narrowing
//!   ([`CastKind::Trunc`]), and sign- or zero-extension when widening according
//!   to the **source** type's signedness ([`CastKind::Sext`] /
//!   [`CastKind::Zext`]).
//! * **Float -> Int** is *saturating* to `[T::MIN, T::MAX]` with NaN mapping to
//!   0 ([`CastKind::FpToSi`] / [`CastKind::FpToUi`]). Note this is **not**
//!   LLVM's raw `fptosi`, which is poison out of range; the saturation is the
//!   backend's obligation when it lowers the op.
//! * **Int -> Float** is [`CastKind::SiToFp`] / [`CastKind::UiToFp`] by the
//!   source's signedness.
//! * **Float -> Float** is [`CastKind::FpTrunc`] / [`CastKind::FpExt`].
//! * **Same machine type on both sides** is [`CastOp::Identity`]: no operation
//!   is emitted at all, the cast lowers to its operand.
//! * `bool` participates **only** as `bool -> integer` ([`CastKind::Zext`]).
//!   Integer -> `bool` is illegal -- use a comparison.
//! * Everything else (structs, tuples, arrays, functions, and -- once they
//!   exist -- pointers) is illegal via `as`; a reinterpreting conversion is
//!   reserved for a future `mem.transmute`.
//!
//! Every legal cast is [`CastMode::Wrapping`]. [`CastMode::Checked`] exists in
//! the IR but is reserved for the refinement-types integration (M7) and is
//! never produced by `as` today.

use codira_mir::{CastKind, CastMode};
use codira_target::abi::TargetDataLayout;

use super::{
    infer::InferTy,
    primitives::{FloatTy, IntTy},
    ResolveBitness, Ty, TyKind,
};
use crate::primitive_type::{FloatBitness, IntBitness};

/// The machine-level operation a legal `as` cast performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastOp {
    /// Source and target have the same machine representation. No operation
    /// is emitted; the cast lowers to its operand.
    Identity,

    /// A real conversion. The mode is always [`CastMode::Wrapping`] for `as`.
    Convert(CastKind, CastMode),
}

/// Why a cast was rejected. Each variant maps to exactly one diagnostic
/// message; see [`InvalidCastReason::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InvalidCastReason {
    /// The target is `bool` and the source is not. Truthiness conversions are
    /// deliberately not a thing.
    ToBool,

    /// The source is `bool` and the target is a floating-point type. `bool`
    /// only converts to integers.
    BoolToFloat,

    /// At least one side is not a primitive numeric type (or `bool`).
    NotNumeric,
}

impl InvalidCastReason {
    /// A complete, human-readable explanation of the rejection that does not
    /// need the types to be rendered (diagnostic messages have no database
    /// access, so they cannot call [`crate::HirDisplay`]).
    pub fn message(self) -> &'static str {
        match self {
            InvalidCastReason::ToBool => {
                "cannot cast to `bool` with `as`; use a comparison instead"
            }
            InvalidCastReason::BoolToFloat => "`bool` can only be cast to an integer type",
            InvalidCastReason::NotNumeric => {
                "`as` can only convert between primitive numeric types"
            }
        }
    }
}

/// The outcome of checking `source as target`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastCheck {
    /// The cast is legal and this is the operation it performs.
    Legal(CastOp),

    /// Nothing is known to be wrong, but an operation cannot be selected
    /// because one of the sides is not concrete yet: an unresolved literal
    /// type variable, an error type, or a diverging (`never`) operand. Type
    /// inference accepts these silently -- a real problem with them has
    /// already been reported elsewhere, and piling on a second diagnostic
    /// would only be noise.
    Undetermined,

    /// The cast is a type error.
    Illegal(InvalidCastReason),
}

/// Checks `source as target` against the legality matrix documented at the
/// top of this module, and selects the operation for legal casts.
///
/// `layout` is needed because `usize`/`isize` have no width until the target
/// is known: whether `usize as u64` is an identity, a truncation, or an
/// extension is a per-target answer.
///
/// Both types are expected to be normalized already (type aliases resolved
/// and inference variables substituted as far as they can be); anything left
/// unresolved yields [`CastCheck::Undetermined`] rather than an error.
pub fn check_cast(source: &Ty, target: &Ty, layout: &TargetDataLayout) -> CastCheck {
    // A `never`-typed operand never actually produces a value to convert, so
    // any target is fine. Checked before classification because `never` is a
    // legitimate *source* but not a legitimate *target*.
    if matches!(source.interned(), TyKind::Never) {
        return CastCheck::Undetermined;
    }

    match (classify(source), classify(target)) {
        // An error or a still-unresolved type on either side.
        (Class::Unknown, _) | (_, Class::Unknown) => CastCheck::Undetermined,

        // Aggregates, functions and `never` targets are never castable. Comes
        // before the `bool` rules so that e.g. `some_struct as bool` reports
        // the aggregate as the problem rather than the `bool`.
        (Class::Other, _) | (_, Class::Other) => CastCheck::Illegal(InvalidCastReason::NotNumeric),

        // `bool` as a source.
        (Class::Bool, Class::Bool) => CastCheck::Legal(CastOp::Identity),
        (Class::Bool, Class::Int(_)) => {
            CastCheck::Legal(CastOp::Convert(CastKind::Zext, CastMode::Wrapping))
        }
        (Class::Bool, Class::Float(_) | Class::FloatVar) => {
            CastCheck::Illegal(InvalidCastReason::BoolToFloat)
        }

        // `bool` as a target, from anything numeric.
        (_, Class::Bool) => CastCheck::Illegal(InvalidCastReason::ToBool),

        // Numeric, but one side is an unresolved integer/float literal. (A
        // target is lowered from a written-out type and so is never a variable
        // in practice; only the source side of this arm is ever taken.)
        (Class::IntVar | Class::FloatVar, _) | (_, Class::IntVar | Class::FloatVar) => {
            CastCheck::Undetermined
        }

        (Class::Int(source), Class::Int(target)) => int_to_int(source, target, layout),
        (Class::Int(source), Class::Float(_)) => CastCheck::Legal(CastOp::Convert(
            if source.signedness.is_signed() {
                CastKind::SiToFp
            } else {
                CastKind::UiToFp
            },
            CastMode::Wrapping,
        )),
        (Class::Float(_), Class::Int(target)) => CastCheck::Legal(CastOp::Convert(
            if target.signedness.is_signed() {
                CastKind::FpToSi
            } else {
                CastKind::FpToUi
            },
            CastMode::Wrapping,
        )),
        (Class::Float(source), Class::Float(target)) => float_to_float(source, target),
    }
}

/// The coarse shape of a type for the purposes of casting.
#[derive(Debug, Clone, Copy)]
enum Class {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    /// An unsuffixed integer literal whose type is not pinned down yet.
    IntVar,
    /// An unsuffixed float literal whose type is not pinned down yet.
    FloatVar,
    /// An error type or an unresolved general type variable.
    Unknown,
    /// Known, and not a primitive numeric type.
    Other,
}

fn classify(ty: &Ty) -> Class {
    match ty.interned() {
        TyKind::Int(int_ty) => Class::Int(*int_ty),
        TyKind::Float(float_ty) => Class::Float(*float_ty),
        TyKind::Bool => Class::Bool,
        TyKind::InferenceVar(InferTy::Int(_)) => Class::IntVar,
        TyKind::InferenceVar(InferTy::Float(_)) => Class::FloatVar,
        // A `TypeAlias` that survived normalization is a cyclic alias, which
        // has already been reported as such.
        TyKind::InferenceVar(InferTy::Type(_)) | TyKind::Unknown | TyKind::TypeAlias(_) => {
            Class::Unknown
        }
        TyKind::Struct(..)
        | TyKind::Tuple(..)
        | TyKind::Array(_)
        | TyKind::FnDef(..)
        // A generic parameter is not a numeric type *here*, whatever it is
        // instantiated with later. `Other` rejects the cast, which is the
        // right answer: `x as T` for an unconstrained `T` cannot be given a
        // conversion until `T` is known, and silently permitting it would
        // mean choosing one arbitrarily.
        | TyKind::TypeParam(..)
        // A string is not a number and casting one to an integer would have
        // to mean something -- its address, its length, its first byte --
        // that the language has not chosen. `Other` rejects it rather than
        // picking.
        | TyKind::Str
        | TyKind::Never => Class::Other,
    }
}

fn int_to_int(source: IntTy, target: IntTy, layout: &TargetDataLayout) -> CastCheck {
    let source = source.resolve(layout);
    let target = target.resolve(layout);
    let (source_width, target_width) = (int_width(source.bitness), int_width(target.bitness));

    let op = if target_width == source_width {
        // Same width. The bit pattern is unchanged either way; when the
        // signedness matches too (`isize as i64` on a 64-bit target) there is
        // nothing at all to emit, otherwise the value is merely reinterpreted.
        // Same width is Identity regardless of signedness. An LLVM
        // integer type carries no sign -- `i32` and `u32` are the *same*
        // machine type, and signedness lives in the HIR `Ty` and in the
        // choice of operations (sdiv vs udiv, sext vs zext). Emitting a
        // bitcast here would be a no-op instruction and, more
        // importantly, a third Int->Int operation that
        // spec/LANGUAGE_SPEC.md section 18.5 does not list.
        CastOp::Identity
    } else if target_width < source_width {
        CastOp::Convert(CastKind::Trunc, CastMode::Wrapping)
    } else if source.signedness.is_signed() {
        CastOp::Convert(CastKind::Sext, CastMode::Wrapping)
    } else {
        CastOp::Convert(CastKind::Zext, CastMode::Wrapping)
    };

    CastCheck::Legal(op)
}

fn float_to_float(source: FloatTy, target: FloatTy) -> CastCheck {
    let (source_width, target_width) = (float_width(source.bitness), float_width(target.bitness));

    let op = if source_width == target_width {
        CastOp::Identity
    } else if target_width < source_width {
        CastOp::Convert(CastKind::FpTrunc, CastMode::Wrapping)
    } else {
        CastOp::Convert(CastKind::FpExt, CastMode::Wrapping)
    };

    CastCheck::Legal(op)
}

fn int_width(bitness: IntBitness) -> u32 {
    match bitness {
        IntBitness::X8 => 8,
        IntBitness::X16 => 16,
        IntBitness::X32 => 32,
        IntBitness::X64 => 64,
        IntBitness::X128 => 128,
        IntBitness::Xsize => {
            unreachable!("variable bitness must be resolved against a target before comparison")
        }
    }
}

fn float_width(bitness: FloatBitness) -> u32 {
    match bitness {
        FloatBitness::X32 => 32,
        FloatBitness::X64 => 64,
    }
}

#[cfg(test)]
mod tests {
    use codira_target::abi::{Size, TargetDataLayout};

    use super::{
        check_cast, CastCheck, CastKind, CastMode, CastOp, FloatTy, IntTy, InvalidCastReason,
    };
    use crate::{
        ty::{infer::test_integer_var, Substitution},
        Ty, TyKind,
    };

    fn layout_64() -> TargetDataLayout {
        TargetDataLayout::default()
    }

    fn layout_32() -> TargetDataLayout {
        TargetDataLayout {
            pointer_size: Size::from_bits(32),
            ..TargetDataLayout::default()
        }
    }

    fn int(ty: IntTy) -> Ty {
        TyKind::Int(ty).intern()
    }

    fn float(ty: FloatTy) -> Ty {
        TyKind::Float(ty).intern()
    }

    fn convert(kind: CastKind) -> CastCheck {
        CastCheck::Legal(CastOp::Convert(kind, CastMode::Wrapping))
    }

    fn check(source: &Ty, target: &Ty) -> CastCheck {
        check_cast(source, target, &layout_64())
    }

    #[test]
    fn int_to_int_is_wrapping() {
        // Narrowing truncates regardless of signedness.
        assert_eq!(
            check(&int(IntTy::i64()), &int(IntTy::i8())),
            convert(CastKind::Trunc)
        );
        assert_eq!(
            check(&int(IntTy::u64()), &int(IntTy::u8())),
            convert(CastKind::Trunc)
        );
        assert_eq!(
            check(&int(IntTy::u64()), &int(IntTy::i8())),
            convert(CastKind::Trunc)
        );

        // Widening extends according to the *source*'s signedness.
        assert_eq!(
            check(&int(IntTy::i8()), &int(IntTy::i64())),
            convert(CastKind::Sext)
        );
        assert_eq!(
            check(&int(IntTy::i8()), &int(IntTy::u64())),
            convert(CastKind::Sext)
        );
        assert_eq!(
            check(&int(IntTy::u8()), &int(IntTy::i64())),
            convert(CastKind::Zext)
        );
        assert_eq!(
            check(&int(IntTy::u8()), &int(IntTy::u64())),
            convert(CastKind::Zext)
        );
    }

    #[test]
    fn same_width_int_casts_emit_no_conversion() {
        // Identical type: nothing at all.
        assert_eq!(
            check(&int(IntTy::i32()), &int(IntTy::i32())),
            CastCheck::Legal(CastOp::Identity)
        );
        // Same width, different signedness: also nothing. An LLVM integer
        // type carries no sign, so i32 and u32 are the same machine type;
        // signedness is a property of the HIR `Ty` and of the operations
        // chosen (sext vs zext, sdiv vs udiv), not of the representation.
        assert_eq!(
            check(&int(IntTy::i32()), &int(IntTy::u32())),
            CastCheck::Legal(CastOp::Identity)
        );
        assert_eq!(
            check(&int(IntTy::u64()), &int(IntTy::i64())),
            CastCheck::Legal(CastOp::Identity)
        );
    }

    #[test]
    fn variable_bitness_is_resolved_against_the_target() {
        let (isize_ty, i32_ty, i64_ty) =
            (int(IntTy::isize()), int(IntTy::i32()), int(IntTy::i64()));

        assert_eq!(
            check_cast(&isize_ty, &i64_ty, &layout_64()),
            CastCheck::Legal(CastOp::Identity)
        );
        assert_eq!(
            check_cast(&isize_ty, &i32_ty, &layout_64()),
            convert(CastKind::Trunc)
        );

        assert_eq!(
            check_cast(&isize_ty, &i32_ty, &layout_32()),
            CastCheck::Legal(CastOp::Identity)
        );
        assert_eq!(
            check_cast(&isize_ty, &i64_ty, &layout_32()),
            convert(CastKind::Sext)
        );
    }

    #[test]
    fn float_conversions_follow_signedness_and_width() {
        assert_eq!(
            check(&int(IntTy::i32()), &float(FloatTy::f64())),
            convert(CastKind::SiToFp)
        );
        assert_eq!(
            check(&int(IntTy::u32()), &float(FloatTy::f64())),
            convert(CastKind::UiToFp)
        );
        assert_eq!(
            check(&float(FloatTy::f64()), &int(IntTy::i32())),
            convert(CastKind::FpToSi)
        );
        assert_eq!(
            check(&float(FloatTy::f64()), &int(IntTy::u32())),
            convert(CastKind::FpToUi)
        );

        assert_eq!(
            check(&float(FloatTy::f64()), &float(FloatTy::f32())),
            convert(CastKind::FpTrunc)
        );
        assert_eq!(
            check(&float(FloatTy::f32()), &float(FloatTy::f64())),
            convert(CastKind::FpExt)
        );
        assert_eq!(
            check(&float(FloatTy::f32()), &float(FloatTy::f32())),
            CastCheck::Legal(CastOp::Identity)
        );
    }

    #[test]
    fn bool_is_only_a_source_and_only_to_integers() {
        let bool_ty = TyKind::Bool.intern();

        assert_eq!(check(&bool_ty, &int(IntTy::i32())), convert(CastKind::Zext));
        assert_eq!(
            check(&bool_ty, &bool_ty),
            CastCheck::Legal(CastOp::Identity)
        );
        assert_eq!(
            check(&bool_ty, &float(FloatTy::f64())),
            CastCheck::Illegal(InvalidCastReason::BoolToFloat)
        );
        assert_eq!(
            check(&int(IntTy::i32()), &bool_ty),
            CastCheck::Illegal(InvalidCastReason::ToBool)
        );
        assert_eq!(
            check(&float(FloatTy::f64()), &bool_ty),
            CastCheck::Illegal(InvalidCastReason::ToBool)
        );
    }

    #[test]
    fn aggregates_are_never_castable() {
        let unit = Ty::unit();
        let array = TyKind::Array(int(IntTy::i32())).intern();
        let never = TyKind::Never.intern();

        assert_eq!(
            check(&unit, &int(IntTy::i32())),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
        assert_eq!(
            check(&array, &int(IntTy::i32())),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
        assert_eq!(
            check(&int(IntTy::i32()), &array),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
        assert_eq!(
            check(
                &int(IntTy::i32()),
                &TyKind::Tuple(1, Substitution::single(int(IntTy::i32()))).intern()
            ),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
        // `never` is a legitimate *source* (the operand diverges) but never a
        // legitimate target.
        assert_eq!(check(&never, &int(IntTy::i32())), CastCheck::Undetermined);
        assert_eq!(
            check(&int(IntTy::i32()), &never),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
    }

    #[test]
    fn unresolved_types_never_produce_an_error() {
        let unknown = TyKind::Unknown.intern();
        let int_var = test_integer_var();

        assert_eq!(check(&unknown, &int(IntTy::i32())), CastCheck::Undetermined);
        assert_eq!(check(&int(IntTy::i32()), &unknown), CastCheck::Undetermined);
        assert_eq!(check(&int_var, &int(IntTy::i32())), CastCheck::Undetermined);
        assert_eq!(
            check(&int_var, &float(FloatTy::f64())),
            CastCheck::Undetermined
        );
        // ... but an unresolved literal still cannot be cast to `bool` or to
        // an aggregate; the illegality does not depend on the exact width.
        assert_eq!(
            check(&int_var, &TyKind::Bool.intern()),
            CastCheck::Illegal(InvalidCastReason::ToBool)
        );
        assert_eq!(
            check(&int_var, &Ty::unit()),
            CastCheck::Illegal(InvalidCastReason::NotNumeric)
        );
    }
}
