//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`Tensor`]: an N-dimensional array over a [`Float`] scalar, stored as a
//!   shared buffer plus shape, strides (in elements) and an offset, so that
//!   transpose, permute, slice, broadcast and most reshapes are zero-copy
//!   views.
//!
//! # Value semantics
//!
//! A `Tensor` is immutable once built: every operation returns a new
//! tensor, and views share their buffer through an `Arc`. The one mutable
//! access, [`Tensor::data_mut`], copies first if the buffer is shared or
//! the layout is not contiguous (copy-on-write), so a view can never
//! observe a write made through another tensor.
//!
//! # Layout
//!
//! Strides are non-negative element counts. The logical order of elements
//! (what [`Tensor::to_vec`] returns and what every kernel iterates in) is
//! row-major over the shape, whatever the strides are. A broadcast view has
//! stride 0 on its broadcast axes.

pub mod matmul;
pub mod ops;
pub mod shape;

use std::{fmt, sync::Arc};

pub use self::shape::{broadcast_shapes, contiguous_strides, numel};
use self::shape::{broadcast_strides, for_each_offset, for_each_offset2, view_strides};
use crate::{
    error::{Result, TensorError},
    float::Float,
    rng::Rng,
};

/// An N-dimensional strided array. See the module documentation.
#[derive(Clone)]
pub struct Tensor<T> {
    data: Arc<Vec<T>>,
    shape: Vec<usize>,
    strides: Vec<usize>,
    offset: usize,
}

impl<T: Float> Tensor<T> {
    // ---------------------------------------------------------------- build

    /// A contiguous tensor holding `data` in row-major order.
    pub fn from_vec(data: Vec<T>, shape: &[usize]) -> Result<Self> {
        let expected = numel(shape);
        if data.len() != expected {
            return Err(TensorError::DataLength {
                expected,
                got: data.len(),
            });
        }
        Ok(Tensor {
            data: Arc::new(data),
            shape: shape.to_vec(),
            strides: contiguous_strides(shape),
            offset: 0,
        })
    }

    /// A rank-0 tensor.
    pub fn scalar(x: T) -> Self {
        Tensor {
            data: Arc::new(vec![x]),
            shape: vec![],
            strides: vec![],
            offset: 0,
        }
    }

    /// Every element equal to `x`.
    pub fn full(shape: &[usize], x: T) -> Self {
        Self::from_vec(vec![x; numel(shape)], shape).expect("length matches by construction")
    }

    /// All zeros.
    pub fn zeros(shape: &[usize]) -> Self {
        Self::full(shape, T::ZERO)
    }

    /// All ones.
    pub fn ones(shape: &[usize]) -> Self {
        Self::full(shape, T::ONE)
    }

    /// Element `i` (in row-major order) is `f(i)`.
    pub fn from_fn(shape: &[usize], f: impl FnMut(usize) -> T) -> Self {
        let data: Vec<T> = (0..numel(shape)).map(f).collect();
        Self::from_vec(data, shape).expect("length matches by construction")
    }

    /// Independent `N(0, std^2)` samples.
    pub fn randn(shape: &[usize], std: f64, rng: &mut Rng) -> Self {
        Self::from_fn(shape, |_| T::from_f64(rng.normal() * std))
    }

    /// Independent uniform samples in `[lo, hi)`.
    pub fn rand_uniform(shape: &[usize], lo: f64, hi: f64, rng: &mut Rng) -> Self {
        Self::from_fn(shape, |_| T::from_f64(rng.uniform(lo, hi)))
    }

    // -------------------------------------------------------------- inspect

    /// The extent of each axis.
    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    /// The stride of each axis, in elements.
    pub fn strides(&self) -> &[usize] {
        &self.strides
    }

    /// The number of axes.
    pub fn rank(&self) -> usize {
        self.shape.len()
    }

    /// The number of elements.
    pub fn numel(&self) -> usize {
        numel(&self.shape)
    }

    /// `true` if the elements are laid out row-major with no gaps.
    pub fn is_contiguous(&self) -> bool {
        let mut expect = 1;
        for i in (0..self.rank()).rev() {
            if self.shape[i] != 1 && self.strides[i] != expect {
                return false;
            }
            expect *= self.shape[i];
        }
        true
    }

    /// The element at a multi-index.
    pub fn get(&self, index: &[usize]) -> Result<T> {
        if index.len() != self.rank() {
            return Err(TensorError::ShapeMismatch {
                op: "get",
                expected: vec![self.rank()],
                got: vec![index.len()],
            });
        }
        let mut off = self.offset;
        for (ax, (&i, &d)) in index.iter().zip(&self.shape).enumerate() {
            if i >= d {
                return Err(TensorError::IndexOutOfRange { index: i, bound: d });
            }
            off += i * self.strides[ax];
        }
        Ok(self.data[off])
    }

    /// The value of a single-element tensor (any rank).
    pub fn item(&self) -> Result<T> {
        if self.numel() != 1 {
            return Err(TensorError::ShapeMismatch {
                op: "item",
                expected: vec![],
                got: self.shape.clone(),
            });
        }
        Ok(self.data[self.offset])
    }

    /// The elements in row-major logical order.
    pub fn to_vec(&self) -> Vec<T> {
        if let Some(s) = self.as_slice() {
            return s.to_vec();
        }
        let mut out = Vec::with_capacity(self.numel());
        for_each_offset(&self.shape, &self.strides, self.offset, |o| {
            out.push(self.data[o]);
        });
        out
    }

    /// The elements as one slice, if the layout is contiguous.
    pub fn as_slice(&self) -> Option<&[T]> {
        if self.is_contiguous() {
            Some(&self.data[self.offset..self.offset + self.numel()])
        } else {
            None
        }
    }

    /// This tensor if contiguous, else a contiguous copy.
    pub fn contiguous(&self) -> Self {
        if self.is_contiguous() {
            self.clone()
        } else {
            Self::from_vec(self.to_vec(), &self.shape).expect("same element count")
        }
    }

    /// Mutable access to the elements in row-major order. Copies first if
    /// the buffer is shared or not contiguous, so no other tensor changes.
    pub fn data_mut(&mut self) -> &mut [T] {
        let compact = self.is_contiguous() && self.offset == 0 && self.data.len() == self.numel();
        if !compact {
            *self = Self::from_vec(self.to_vec(), &self.shape).expect("same element count");
        }
        Arc::make_mut(&mut self.data).as_mut_slice()
    }

    /// Converts every element to another scalar type.
    pub fn cast<U: Float>(&self) -> Tensor<U> {
        Tensor::from_vec(
            self.to_vec()
                .into_iter()
                .map(|x| U::from_f64(x.to_f64()))
                .collect(),
            &self.shape,
        )
        .expect("same element count")
    }

    /// `true` if shapes are equal and every pair of elements satisfies
    /// `|a - b| <= atol + rtol * |b|`.
    pub fn allclose(&self, other: &Self, rtol: f64, atol: f64) -> bool {
        self.shape == other.shape
            && self.to_vec().iter().zip(other.to_vec()).all(|(a, b)| {
                let (a, b) = (a.to_f64(), b.to_f64());
                (a - b).abs() <= atol + rtol * b.abs()
            })
    }

    /// The largest `|a - b|` over corresponding elements (shapes must match).
    pub fn max_abs_diff(&self, other: &Self) -> Result<f64> {
        if self.shape != other.shape {
            return Err(TensorError::ShapeMismatch {
                op: "max_abs_diff",
                expected: self.shape.clone(),
                got: other.shape.clone(),
            });
        }
        Ok(self
            .to_vec()
            .iter()
            .zip(other.to_vec())
            .map(|(a, b)| (a.to_f64() - b.to_f64()).abs())
            .fold(0.0, f64::max))
    }

    // ---------------------------------------------------------------- views

    fn check_axis(&self, axis: usize) -> Result<()> {
        if axis >= self.rank() {
            Err(TensorError::AxisOutOfRange {
                axis,
                rank: self.rank(),
            })
        } else {
            Ok(())
        }
    }

    /// Reorders the axes: axis `i` of the result is axis `perm[i]` of `self`.
    pub fn permute(&self, perm: &[usize]) -> Result<Self> {
        let rank = self.rank();
        let mut seen = vec![false; rank];
        if perm.len() != rank
            || perm
                .iter()
                .any(|&p| p >= rank || std::mem::replace(&mut seen[p], true))
        {
            return Err(TensorError::InvalidPermutation {
                perm: perm.to_vec(),
                rank,
            });
        }
        Ok(Tensor {
            data: self.data.clone(),
            shape: perm.iter().map(|&p| self.shape[p]).collect(),
            strides: perm.iter().map(|&p| self.strides[p]).collect(),
            offset: self.offset,
        })
    }

    /// Swaps two axes (a view).
    pub fn transpose(&self, a: usize, b: usize) -> Result<Self> {
        self.check_axis(a)?;
        self.check_axis(b)?;
        let mut perm: Vec<usize> = (0..self.rank()).collect();
        perm.swap(a, b);
        self.permute(&perm)
    }

    /// Swaps the last two axes (the matrix transpose of every batch entry).
    pub fn t(&self) -> Result<Self> {
        if self.rank() < 2 {
            return Err(TensorError::RankTooLow {
                op: "t",
                needed: 2,
                got: self.rank(),
            });
        }
        self.transpose(self.rank() - 2, self.rank() - 1)
    }

    /// The sub-range `start..end` of one axis (a view).
    pub fn slice(&self, axis: usize, start: usize, end: usize) -> Result<Self> {
        self.check_axis(axis)?;
        let dim = self.shape[axis];
        if start > end || end > dim {
            return Err(TensorError::SliceOutOfRange {
                axis,
                start,
                end,
                dim,
            });
        }
        let mut shape = self.shape.clone();
        shape[axis] = end - start;
        Ok(Tensor {
            data: self.data.clone(),
            offset: self.offset + start * self.strides[axis],
            shape,
            strides: self.strides.clone(),
        })
    }

    /// A zero-copy reshape. Fails with [`TensorError::ReshapeCount`] if the
    /// element count changes and [`TensorError::NotAView`] if the strides
    /// cannot express the new shape.
    pub fn view(&self, new_shape: &[usize]) -> Result<Self> {
        if numel(new_shape) != self.numel() {
            return Err(TensorError::ReshapeCount {
                from: self.shape.clone(),
                to: new_shape.to_vec(),
            });
        }
        match view_strides(&self.shape, &self.strides, new_shape) {
            Some(strides) => Ok(Tensor {
                data: self.data.clone(),
                shape: new_shape.to_vec(),
                strides,
                offset: self.offset,
            }),
            None => Err(TensorError::NotAView {
                shape: self.shape.clone(),
                strides: self.strides.clone(),
                to: new_shape.to_vec(),
            }),
        }
    }

    /// Reshape: a view when the layout allows it, otherwise a contiguous
    /// copy. Fails only if the element count changes.
    pub fn reshape(&self, new_shape: &[usize]) -> Result<Self> {
        match self.view(new_shape) {
            Err(TensorError::NotAView { .. }) => self.contiguous().view(new_shape),
            r => r,
        }
    }

    /// A view that reads as `shape`, repeating along broadcast axes.
    pub fn broadcast_to(&self, shape: &[usize]) -> Result<Self> {
        let out = broadcast_shapes("broadcast_to", &self.shape, shape)?;
        if out != shape {
            return Err(TensorError::ShapeMismatch {
                op: "broadcast_to",
                expected: shape.to_vec(),
                got: self.shape.clone(),
            });
        }
        Ok(Tensor {
            data: self.data.clone(),
            strides: broadcast_strides(&self.shape, &self.strides, shape),
            shape: shape.to_vec(),
            offset: self.offset,
        })
    }

    // ------------------------------------------------------- element-wise

    /// Applies `f` to every element.
    pub fn map(&self, f: impl Fn(T) -> T) -> Self {
        let data = match self.as_slice() {
            Some(s) => s.iter().map(|&x| f(x)).collect(),
            None => self.to_vec().into_iter().map(f).collect(),
        };
        Self::from_vec(data, &self.shape).expect("same element count")
    }

    /// Applies `f` element-wise after broadcasting the two shapes together.
    pub fn zip_map(&self, other: &Self, op: &'static str, f: impl Fn(T, T) -> T) -> Result<Self> {
        if self.shape == other.shape {
            if let (Some(a), Some(b)) = (self.as_slice(), other.as_slice()) {
                let data = a.iter().zip(b).map(|(&x, &y)| f(x, y)).collect();
                return Self::from_vec(data, &self.shape);
            }
        }
        let out_shape = broadcast_shapes(op, &self.shape, &other.shape)?;
        let sa = broadcast_strides(&self.shape, &self.strides, &out_shape);
        let sb = broadcast_strides(&other.shape, &other.strides, &out_shape);
        let mut data = Vec::with_capacity(numel(&out_shape));
        for_each_offset2(
            &out_shape,
            (&sa, self.offset),
            (&sb, other.offset),
            |i, j| data.push(f(self.data[i], other.data[j])),
        );
        Self::from_vec(data, &out_shape)
    }
}

impl<T: Float> fmt::Debug for Tensor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Tensor(shape={:?}, data={:?})",
            self.shape,
            self.to_vec()
        )
    }
}

impl<T: Float> PartialEq for Tensor<T> {
    /// Equal shapes and equal elements in logical order (layout ignored).
    fn eq(&self, other: &Self) -> bool {
        self.shape == other.shape && self.to_vec() == other.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(data: &[f64], shape: &[usize]) -> Tensor<f64> {
        Tensor::from_vec(data.to_vec(), shape).unwrap()
    }

    #[test]
    fn from_vec_checks_length() {
        assert_eq!(
            Tensor::<f32>::from_vec(vec![1.0; 5], &[2, 3]).unwrap_err(),
            TensorError::DataLength {
                expected: 6,
                got: 5
            }
        );
    }

    #[test]
    fn transpose_is_a_view_with_swapped_strides() {
        let a = t(&[1., 2., 3., 4., 5., 6.], &[2, 3]);
        let at = a.t().unwrap();
        assert_eq!(at.shape(), &[3, 2]);
        assert_eq!(at.strides(), &[1, 3]);
        assert!(!at.is_contiguous());
        assert_eq!(at.to_vec(), vec![1., 4., 2., 5., 3., 6.]);
        assert_eq!(at.get(&[2, 1]).unwrap(), 6.0);
    }

    #[test]
    fn slice_offsets_into_the_buffer() {
        let a = Tensor::<f64>::from_fn(&[3, 4], |i| i as f64);
        let s = a.slice(1, 1, 3).unwrap();
        assert_eq!(s.shape(), &[3, 2]);
        assert_eq!(s.to_vec(), vec![1., 2., 5., 6., 9., 10.]);
        let s2 = s.slice(0, 2, 3).unwrap();
        assert_eq!(s2.to_vec(), vec![9., 10.]);
        assert!(matches!(
            a.slice(0, 2, 4),
            Err(TensorError::SliceOutOfRange { .. })
        ));
        assert!(matches!(
            a.slice(2, 0, 1),
            Err(TensorError::AxisOutOfRange { .. })
        ));
    }

    #[test]
    fn reshape_errors_instead_of_panicking() {
        let a = Tensor::<f64>::zeros(&[2, 3]);
        assert!(matches!(
            a.reshape(&[4, 2]),
            Err(TensorError::ReshapeCount { .. })
        ));
        // a transposed matrix cannot be flattened without a copy
        let at = a.t().unwrap();
        assert!(matches!(at.view(&[6]), Err(TensorError::NotAView { .. })));
        // reshape copies in that case and keeps logical order
        let b = Tensor::<f64>::from_fn(&[2, 3], |i| i as f64).t().unwrap();
        assert_eq!(
            b.reshape(&[6]).unwrap().to_vec(),
            vec![0., 3., 1., 4., 2., 5.]
        );
    }

    #[test]
    fn view_of_contiguous_shares_the_buffer() {
        let a = Tensor::<f64>::from_fn(&[2, 6], |i| i as f64);
        let v = a.view(&[3, 2, 2]).unwrap();
        assert!(Arc::ptr_eq(&a.data, &v.data));
        assert_eq!(v.get(&[2, 1, 0]).unwrap(), 10.0);
    }

    #[test]
    fn invalid_permutations_are_rejected() {
        let a = Tensor::<f64>::zeros(&[2, 3, 4]);
        assert!(a.permute(&[0, 0, 1]).is_err());
        assert!(a.permute(&[0, 1]).is_err());
        assert_eq!(a.permute(&[2, 0, 1]).unwrap().shape(), &[4, 2, 3]);
    }

    #[test]
    fn broadcast_to_uses_zero_strides() {
        let a = t(&[1., 2., 3.], &[3]);
        let b = a.broadcast_to(&[2, 3]).unwrap();
        assert_eq!(b.strides(), &[0, 1]);
        assert_eq!(b.to_vec(), vec![1., 2., 3., 1., 2., 3.]);
        assert!(a.broadcast_to(&[3, 2]).is_err());
    }

    #[test]
    fn data_mut_is_copy_on_write() {
        let a = t(&[1., 2., 3., 4.], &[2, 2]);
        let mut b = a.clone();
        b.data_mut()[0] = 9.0;
        assert_eq!(a.to_vec()[0], 1.0);
        assert_eq!(b.to_vec()[0], 9.0);
        let mut c = a.t().unwrap();
        c.data_mut()[1] = 7.0;
        assert_eq!(c.to_vec(), vec![1., 7., 2., 4.]);
        assert_eq!(a.to_vec(), vec![1., 2., 3., 4.]);
    }

    #[test]
    fn zip_map_broadcasts_views() {
        let a = Tensor::<f64>::from_fn(&[3, 2], |i| i as f64).t().unwrap(); // [2,3]
        let b = t(&[10., 20.], &[2, 1]);
        let c = a.zip_map(&b, "add", |x, y| x + y).unwrap();
        assert_eq!(c.shape(), &[2, 3]);
        assert_eq!(c.to_vec(), vec![10., 12., 14., 21., 23., 25.]);
        let bad = t(&[1., 2.], &[2]);
        assert!(matches!(
            a.zip_map(&bad, "add", |x, y| x + y),
            Err(TensorError::Broadcast { .. })
        ));
    }
}
