//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`Dual`]: dual numbers `a + b*eps` with `eps^2 = 0`, for forward-mode
//!   derivatives. `Dual` implements [`Float`], so every tensor kernel in the
//!   crate also runs on `Tensor<Dual>` and yields a Jacobian-vector product
//!   in one forward pass ([`jvp`]).
//! - [`derivative`]: `f(x)` and `f'(x)` of a scalar function.
//!
//! Comparisons (`<`, `max`, the `relu` test) look at the real part only,
//! so piecewise functions differentiate as the branch taken at `x`.

use std::{
    fmt,
    ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign},
};

use crate::{error::Result, float::Float, tensor::Tensor};

/// `re + du * eps`, `eps^2 = 0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Dual {
    /// The value.
    pub re: f64,
    /// The derivative (tangent) part.
    pub du: f64,
}

impl Dual {
    /// `re + du * eps`.
    pub const fn new(re: f64, du: f64) -> Self {
        Dual { re, du }
    }
}

impl PartialOrd for Dual {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.re.partial_cmp(&other.re)
    }
}

impl fmt::Display for Dual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{}e", self.re, self.du)
    }
}

impl Add for Dual {
    type Output = Dual;
    fn add(self, o: Dual) -> Dual {
        Dual::new(self.re + o.re, self.du + o.du)
    }
}
impl Sub for Dual {
    type Output = Dual;
    fn sub(self, o: Dual) -> Dual {
        Dual::new(self.re - o.re, self.du - o.du)
    }
}
impl Mul for Dual {
    type Output = Dual;
    fn mul(self, o: Dual) -> Dual {
        Dual::new(self.re * o.re, self.re * o.du + self.du * o.re)
    }
}
impl Div for Dual {
    type Output = Dual;
    fn div(self, o: Dual) -> Dual {
        Dual::new(
            self.re / o.re,
            (self.du * o.re - self.re * o.du) / (o.re * o.re),
        )
    }
}
impl Neg for Dual {
    type Output = Dual;
    fn neg(self) -> Dual {
        Dual::new(-self.re, -self.du)
    }
}
impl AddAssign for Dual {
    fn add_assign(&mut self, o: Dual) {
        *self = *self + o;
    }
}
impl SubAssign for Dual {
    fn sub_assign(&mut self, o: Dual) {
        *self = *self - o;
    }
}
impl MulAssign for Dual {
    fn mul_assign(&mut self, o: Dual) {
        *self = *self * o;
    }
}
impl DivAssign for Dual {
    fn div_assign(&mut self, o: Dual) {
        *self = *self / o;
    }
}

impl Float for Dual {
    const ZERO: Self = Dual::new(0.0, 0.0);
    const ONE: Self = Dual::new(1.0, 0.0);
    const NEG_INFINITY: Self = Dual::new(f64::NEG_INFINITY, 0.0);

    fn from_f64(x: f64) -> Self {
        Dual::new(x, 0.0)
    }
    fn to_f64(self) -> f64 {
        self.re
    }
    fn exp(self) -> Self {
        let e = self.re.exp();
        Dual::new(e, e * self.du)
    }
    fn ln(self) -> Self {
        Dual::new(self.re.ln(), self.du / self.re)
    }
    fn tanh(self) -> Self {
        let t = self.re.tanh();
        Dual::new(t, (1.0 - t * t) * self.du)
    }
    fn sqrt(self) -> Self {
        let s = self.re.sqrt();
        Dual::new(s, self.du / (2.0 * s))
    }
    fn abs(self) -> Self {
        if self.re < 0.0 {
            -self
        } else {
            self
        }
    }
    fn is_finite(self) -> bool {
        self.re.is_finite() && self.du.is_finite()
    }
    fn is_nan_like(self) -> bool {
        self.re.is_nan()
    }
}

/// `(f(x), f'(x))` for a scalar function, in one evaluation.
pub fn derivative(f: impl Fn(Dual) -> Dual, x: f64) -> (f64, f64) {
    let y = f(Dual::new(x, 1.0));
    (y.re, y.du)
}

/// The Jacobian-vector product of a tensor function: `(f(x), J_f(x) v)`.
pub fn jvp(
    f: impl Fn(&Tensor<Dual>) -> Result<Tensor<Dual>>,
    x: &Tensor<f64>,
    v: &Tensor<f64>,
) -> Result<(Tensor<f64>, Tensor<f64>)> {
    if x.shape() != v.shape() {
        return Err(crate::TensorError::ShapeMismatch {
            op: "jvp",
            expected: x.shape().to_vec(),
            got: v.shape().to_vec(),
        });
    }
    let duals: Vec<Dual> = x
        .to_vec()
        .into_iter()
        .zip(v.to_vec())
        .map(|(a, b)| Dual::new(a, b))
        .collect();
    let y = f(&Tensor::from_vec(duals, x.shape())?)?;
    let parts = y.to_vec();
    Ok((
        Tensor::from_vec(parts.iter().map(|d| d.re).collect(), y.shape())?,
        Tensor::from_vec(parts.iter().map(|d| d.du).collect(), y.shape())?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_derivative_matches_closed_form() {
        // f(x) = tanh(exp(x) / (1 + x^2)) + sqrt(x) * ln(x)
        let f = |x: Dual| (x.exp() / (Dual::ONE + x * x)).tanh() + x.sqrt() * x.ln();
        for &x in &[0.3, 1.0, 2.5] {
            let (y, dy) = derivative(f, x);
            let u: f64 = x.exp() / (1.0 + x * x);
            let du = x.exp() * (1.0 + x * x - 2.0 * x) / (1.0 + x * x).powi(2);
            let expect = (1.0 - u.tanh().powi(2)) * du + 0.5 / x.sqrt() * x.ln() + x.sqrt() / x;
            assert!((y - (u.tanh() + x.sqrt() * x.ln())).abs() < 1e-14);
            assert!((dy - expect).abs() < 1e-12, "{dy} vs {expect}");
        }
    }
}
