//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The [`Float`] trait: the scalar operations every tensor kernel needs,
//!   implemented for `f32` and `f64` (and, in `autograd::dual`, for dual
//!   numbers).
//!
//! Every method is a direct call to the corresponding `std` function; no
//! method approximates or reorders anything, so a kernel written against
//! `Float` computes exactly what the same kernel written against `f32` or
//! `f64` would.

use std::{
    fmt::{Debug, Display},
    ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign},
};

/// A real scalar type a [`crate::Tensor`] can hold.
pub trait Float:
    Copy
    + Debug
    + Display
    + Default
    + PartialEq
    + PartialOrd
    + Send
    + Sync
    + 'static
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
    + DivAssign
{
    /// Additive identity.
    const ZERO: Self;
    /// Multiplicative identity.
    const ONE: Self;
    /// Negative infinity (used as the identity of `max` and for masks).
    const NEG_INFINITY: Self;

    /// Conversion from `f64`, rounding to nearest for `f32`.
    fn from_f64(x: f64) -> Self;
    /// Conversion to `f64` (exact for `f32` and `f64`).
    fn to_f64(self) -> f64;
    /// `e^self`.
    fn exp(self) -> Self;
    /// Natural logarithm.
    fn ln(self) -> Self;
    /// Hyperbolic tangent.
    fn tanh(self) -> Self;
    /// Square root.
    fn sqrt(self) -> Self;
    /// Absolute value.
    fn abs(self) -> Self;
    /// `true` unless NaN or infinite.
    fn is_finite(self) -> bool;

    /// Conversion from a count.
    fn from_usize(n: usize) -> Self {
        Self::from_f64(n as f64)
    }
    /// The larger of two values; NaN in `self` is propagated.
    fn max(self, other: Self) -> Self {
        if self.is_nan_like() || self >= other {
            self
        } else {
            other
        }
    }
    /// The smaller of two values; NaN in `self` is propagated.
    fn min(self, other: Self) -> Self {
        if self.is_nan_like() || self <= other {
            self
        } else {
            other
        }
    }
    /// `true` for NaN (a value not equal to itself).
    #[allow(clippy::eq_op)]
    fn is_nan_like(self) -> bool {
        self != self
    }
}

macro_rules! impl_float {
    ($t:ty) => {
        impl Float for $t {
            const ZERO: Self = 0.0;
            const ONE: Self = 1.0;
            const NEG_INFINITY: Self = <$t>::NEG_INFINITY;

            #[inline]
            fn from_f64(x: f64) -> Self {
                x as $t
            }
            #[inline]
            fn to_f64(self) -> f64 {
                f64::from(self)
            }
            #[inline]
            fn exp(self) -> Self {
                <$t>::exp(self)
            }
            #[inline]
            fn ln(self) -> Self {
                <$t>::ln(self)
            }
            #[inline]
            fn tanh(self) -> Self {
                <$t>::tanh(self)
            }
            #[inline]
            fn sqrt(self) -> Self {
                <$t>::sqrt(self)
            }
            #[inline]
            fn abs(self) -> Self {
                <$t>::abs(self)
            }
            #[inline]
            fn is_finite(self) -> bool {
                <$t>::is_finite(self)
            }
        }
    };
}

impl_float!(f32);
impl_float!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_and_min_propagate_nan_from_self() {
        assert!(Float::max(f64::NAN, 1.0).is_nan());
        assert_eq!(Float::max(2.0f32, 1.0), 2.0);
        assert_eq!(Float::min(2.0f32, 1.0), 1.0);
    }

    #[test]
    fn conversions_round_trip_exactly_for_f32() {
        let x = 0.1f32;
        assert_eq!(f32::from_f64(x.to_f64()), x);
    }
}
