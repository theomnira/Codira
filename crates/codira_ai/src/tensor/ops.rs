//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Element-wise arithmetic with broadcasting, the unary functions the
//!   autograd rules need, reductions along an axis (sum, mean, max), the
//!   gradient helper `reduce_to_shape`, and numerically stable softmax and
//!   log-softmax.
//!
//! # Summation order
//!
//! Every reduction adds its elements strictly in increasing index order
//! along the reduced axis (or in row-major order for whole-tensor
//! reductions). Nothing here reassociates a sum, so results are
//! reproducible across runs and machines for the same inputs. Sequential
//! summation has an error bound that grows linearly with the length; that
//! is the price of the reproducibility and is stated, not hidden.

use super::{
    shape::{broadcast_shapes, for_each_offset},
    Tensor,
};
use crate::{
    error::{Result, TensorError},
    float::Float,
};

impl<T: Float> Tensor<T> {
    /// `self + other`, broadcasting.
    pub fn add(&self, other: &Self) -> Result<Self> {
        self.zip_map(other, "add", |a, b| a + b)
    }
    /// `self - other`, broadcasting.
    pub fn sub(&self, other: &Self) -> Result<Self> {
        self.zip_map(other, "sub", |a, b| a - b)
    }
    /// `self * other` element-wise, broadcasting.
    pub fn mul(&self, other: &Self) -> Result<Self> {
        self.zip_map(other, "mul", |a, b| a * b)
    }
    /// `self / other` element-wise, broadcasting.
    pub fn div(&self, other: &Self) -> Result<Self> {
        self.zip_map(other, "div", |a, b| a / b)
    }
    /// `-self`.
    pub fn neg(&self) -> Self {
        self.map(|x| -x)
    }
    /// `c * self`.
    pub fn scale(&self, c: T) -> Self {
        self.map(|x| x * c)
    }
    /// `self + c`.
    pub fn add_scalar(&self, c: T) -> Self {
        self.map(|x| x + c)
    }
    /// `e^x`.
    pub fn exp(&self) -> Self {
        self.map(T::exp)
    }
    /// `ln x`.
    pub fn ln(&self) -> Self {
        self.map(T::ln)
    }
    /// `tanh x`.
    pub fn tanh(&self) -> Self {
        self.map(T::tanh)
    }
    /// `sqrt x`.
    pub fn sqrt(&self) -> Self {
        self.map(T::sqrt)
    }
    /// `max(x, 0)`.
    pub fn relu(&self) -> Self {
        self.map(|x| if x > T::ZERO { x } else { T::ZERO })
    }
    /// The logistic function, evaluated without overflow for either sign:
    /// `1 / (1 + e^-x)` for `x >= 0`, `e^x / (1 + e^x)` otherwise.
    pub fn sigmoid(&self) -> Self {
        self.map(sigmoid_scalar)
    }

    // ----------------------------------------------------------- reductions

    /// Reduces one axis with `fold`, starting from `init`, visiting the
    /// axis in increasing order. The axis is kept with extent 1 if `keepdim`.
    pub fn reduce_axis(
        &self,
        axis: usize,
        keepdim: bool,
        init: T,
        fold: impl Fn(T, T) -> T,
    ) -> Result<Self> {
        if axis >= self.rank() {
            return Err(TensorError::AxisOutOfRange {
                axis,
                rank: self.rank(),
            });
        }
        let mut outer = self.shape.clone();
        let len = outer[axis];
        outer[axis] = 1;
        let step = self.strides[axis];
        let mut data = Vec::with_capacity(super::numel(&outer));
        for_each_offset(&outer, &self.strides, self.offset, |o| {
            let mut acc = init;
            for i in 0..len {
                acc = fold(acc, self.data[o + i * step]);
            }
            data.push(acc);
        });
        let mut shape = outer;
        if !keepdim {
            shape.remove(axis);
        }
        Tensor::from_vec(data, &shape)
    }

    /// Sum along an axis.
    pub fn sum_axis(&self, axis: usize, keepdim: bool) -> Result<Self> {
        self.reduce_axis(axis, keepdim, T::ZERO, |a, b| a + b)
    }

    /// Mean along an axis.
    pub fn mean_axis(&self, axis: usize, keepdim: bool) -> Result<Self> {
        let n = *self.shape.get(axis).ok_or(TensorError::AxisOutOfRange {
            axis,
            rank: self.rank(),
        })?;
        Ok(self
            .sum_axis(axis, keepdim)?
            .scale(T::ONE / T::from_usize(n)))
    }

    /// Maximum along an axis (`-inf` for an empty axis; NaN propagates).
    pub fn max_axis(&self, axis: usize, keepdim: bool) -> Result<Self> {
        self.reduce_axis(axis, keepdim, T::NEG_INFINITY, |a, b| {
            if b.is_nan_like() {
                b
            } else {
                a.max(b)
            }
        })
    }

    /// The sum of every element, in row-major order.
    pub fn sum_all(&self) -> T {
        self.to_vec().into_iter().fold(T::ZERO, |a, b| a + b)
    }

    /// The mean of every element.
    pub fn mean_all(&self) -> T {
        self.sum_all() / T::from_usize(self.numel())
    }

    /// The maximum element (`-inf` if empty).
    pub fn max_all(&self) -> T {
        self.to_vec()
            .into_iter()
            .fold(T::NEG_INFINITY, super::super::float::Float::max)
    }

    /// Sums a gradient of broadcast shape back to `target`, the shape of
    /// the operand that was broadcast: over the leading axes `target` does
    /// not have, and over the axes where `target` has extent 1.
    pub fn reduce_to_shape(&self, target: &[usize]) -> Result<Self> {
        if self.shape == target {
            return Ok(self.clone());
        }
        let out = broadcast_shapes("reduce_to_shape", target, &self.shape)?;
        if out != self.shape {
            return Err(TensorError::ShapeMismatch {
                op: "reduce_to_shape",
                expected: out,
                got: self.shape.clone(),
            });
        }
        let lead = self.rank() - target.len();
        let mut g = self.clone();
        for ax in 0..self.rank() {
            if ax < lead || (target[ax - lead] == 1 && self.shape[ax] != 1) {
                g = g.sum_axis(ax, true)?;
            }
        }
        g.reshape(target)
    }

    // -------------------------------------------------------------- softmax

    /// `exp(x - max) / sum(exp(x - max))` along `axis`. Subtracting the
    /// maximum makes every exponent `<= 0`, so nothing overflows.
    pub fn softmax(&self, axis: usize) -> Result<Self> {
        let m = self.max_axis(axis, true)?;
        let e = self.sub(&m)?.exp();
        let s = e.sum_axis(axis, true)?;
        e.div(&s)
    }

    /// `x - max - ln(sum(exp(x - max)))` along `axis`: the log of
    /// [`Tensor::softmax`] without ever forming a probability that could
    /// underflow to zero.
    pub fn log_softmax(&self, axis: usize) -> Result<Self> {
        let m = self.max_axis(axis, true)?;
        let shifted = self.sub(&m)?;
        let lse = shifted.exp().sum_axis(axis, true)?.ln();
        shifted.sub(&lse)
    }
}

/// The logistic function for one scalar, stable for large `|x|`.
pub fn sigmoid_scalar<T: Float>(x: T) -> T {
    if x >= T::ZERO {
        T::ONE / (T::ONE + (-x).exp())
    } else {
        let e = x.exp();
        e / (T::ONE + e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(data: &[f64], shape: &[usize]) -> Tensor<f64> {
        Tensor::from_vec(data.to_vec(), shape).unwrap()
    }

    #[test]
    fn reductions_along_each_axis() {
        let a = Tensor::<f64>::from_fn(&[2, 3], |i| i as f64); // [[0,1,2],[3,4,5]]
        assert_eq!(a.sum_axis(0, false).unwrap().to_vec(), vec![3., 5., 7.]);
        assert_eq!(a.sum_axis(1, false).unwrap().to_vec(), vec![3., 12.]);
        assert_eq!(a.sum_axis(1, true).unwrap().shape(), &[2, 1]);
        assert_eq!(a.mean_axis(1, false).unwrap().to_vec(), vec![1., 4.]);
        assert_eq!(a.max_axis(0, false).unwrap().to_vec(), vec![3., 4., 5.]);
        assert_eq!(a.sum_all(), 15.0);
        assert_eq!(a.max_all(), 5.0);
        assert!(a.sum_axis(2, false).is_err());
    }

    #[test]
    fn reductions_respect_views() {
        let a = Tensor::<f64>::from_fn(&[2, 3], |i| i as f64).t().unwrap(); // [3,2]
        assert_eq!(a.sum_axis(1, false).unwrap().to_vec(), vec![3., 5., 7.]);
        let s = Tensor::<f64>::from_fn(&[4, 4], |i| i as f64)
            .slice(1, 1, 3)
            .unwrap();
        assert_eq!(s.sum_axis(0, false).unwrap().to_vec(), vec![28., 32.]);
    }

    #[test]
    fn max_propagates_nan() {
        let a = t(&[1., f64::NAN, 3.], &[3]);
        assert!(a.max_axis(0, false).unwrap().to_vec()[0].is_nan());
    }

    #[test]
    fn reduce_to_shape_sums_broadcast_axes() {
        let g = Tensor::<f64>::ones(&[2, 3, 4]);
        assert_eq!(g.reduce_to_shape(&[4]).unwrap().to_vec(), vec![6.; 4]);
        assert_eq!(g.reduce_to_shape(&[3, 1]).unwrap().to_vec(), vec![8.; 3]);
        assert_eq!(g.reduce_to_shape(&[2, 1, 4]).unwrap().shape(), &[2, 1, 4]);
        assert!(g.reduce_to_shape(&[5]).is_err());
    }

    #[test]
    fn softmax_is_stable_and_normalised() {
        let a = t(&[1000., 1001., 1002., -1000., 0., 1000.], &[2, 3]);
        let s = a.softmax(1).unwrap();
        for row in s.to_vec().chunks(3) {
            assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-12);
            assert!(row.iter().all(|x| x.is_finite()));
        }
        let ls = a.log_softmax(1).unwrap();
        assert!(ls.to_vec().iter().all(|x| x.is_finite()));
        // log_softmax == ln(softmax) where softmax does not underflow
        let r = t(&[0.3, -1.2, 2.0], &[3]);
        assert!(r
            .log_softmax(0)
            .unwrap()
            .allclose(&r.softmax(0).unwrap().ln(), 1e-12, 1e-12));
    }

    #[test]
    fn sigmoid_does_not_overflow() {
        let a = t(&[-1000., 0., 1000.], &[3]).sigmoid();
        assert_eq!(a.to_vec(), vec![0., 0.5, 1.]);
    }
}
