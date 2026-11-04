//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`TensorError`]: every recoverable failure in the crate. Shape errors,
//!   illegal reshapes and out-of-range indices are values, not panics, so a
//!   runtime embedding this crate can turn them into Codira errors.

use std::fmt;

/// The crate's result type.
pub type Result<T> = std::result::Result<T, TensorError>;

/// A failure of a tensor, autograd, layer or quantisation operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TensorError {
    /// `data.len()` did not match the product of the shape.
    DataLength { expected: usize, got: usize },
    /// Two shapes cannot be broadcast together: at `axis` (counted from the
    /// right, 0 = last) the extents differ and neither is 1.
    Broadcast {
        op: &'static str,
        lhs: Vec<usize>,
        rhs: Vec<usize>,
        axis_from_right: usize,
    },
    /// Shapes that must be equal are not.
    ShapeMismatch {
        op: &'static str,
        expected: Vec<usize>,
        got: Vec<usize>,
    },
    /// A reshape changes the number of elements.
    ReshapeCount { from: Vec<usize>, to: Vec<usize> },
    /// A zero-copy view was asked of a layout that cannot provide one.
    NotAView { shape: Vec<usize>, strides: Vec<usize>, to: Vec<usize> },
    /// An axis argument is not less than the rank.
    AxisOutOfRange { axis: usize, rank: usize },
    /// A permutation is not a permutation of `0..rank`.
    InvalidPermutation { perm: Vec<usize>, rank: usize },
    /// A slice `start..end` does not fit in a dimension of extent `dim`.
    SliceOutOfRange {
        axis: usize,
        start: usize,
        end: usize,
        dim: usize,
    },
    /// An index (multi-index, class id, token id) is out of range.
    IndexOutOfRange { index: usize, bound: usize },
    /// The operation needs at least `needed` dimensions.
    RankTooLow { op: &'static str, needed: usize, got: usize },
    /// Matrix multiplication with inner dimensions that differ.
    MatmulInner { lhs: Vec<usize>, rhs: Vec<usize> },
    /// `backward` was called on a tensor that is not a single element.
    NonScalarLoss { shape: Vec<usize> },
    /// A custom gradient returned the wrong number or shape of gradients.
    BadCustomGradient { detail: String },
    /// An argument outside its domain (e.g. a vocabulary smaller than 256).
    InvalidArgument { detail: String },
}

impl fmt::Display for TensorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use TensorError::*;
        match self {
            DataLength { expected, got } => {
                write!(f, "data has {got} elements but the shape needs {expected}")
            }
            Broadcast {
                op,
                lhs,
                rhs,
                axis_from_right,
            } => write!(
                f,
                "{op}: shapes {lhs:?} and {rhs:?} cannot be broadcast \
                 (dimension {axis_from_right} from the right differs and neither is 1)"
            ),
            ShapeMismatch { op, expected, got } => {
                write!(f, "{op}: expected shape {expected:?}, got {got:?}")
            }
            ReshapeCount { from, to } => write!(
                f,
                "cannot reshape {from:?} ({} elements) to {to:?} ({} elements)",
                from.iter().product::<usize>(),
                to.iter().product::<usize>()
            ),
            NotAView { shape, strides, to } => write!(
                f,
                "a tensor of shape {shape:?} with strides {strides:?} cannot be viewed as \
                 {to:?} without copying; use reshape, which copies when it must"
            ),
            AxisOutOfRange { axis, rank } => {
                write!(f, "axis {axis} is out of range for a rank-{rank} tensor")
            }
            InvalidPermutation { perm, rank } => {
                write!(f, "{perm:?} is not a permutation of 0..{rank}")
            }
            SliceOutOfRange {
                axis,
                start,
                end,
                dim,
            } => write!(
                f,
                "slice {start}..{end} does not fit axis {axis} of extent {dim}"
            ),
            IndexOutOfRange { index, bound } => {
                write!(f, "index {index} is out of range (bound {bound})")
            }
            RankTooLow { op, needed, got } => {
                write!(f, "{op} needs rank >= {needed}, got rank {got}")
            }
            MatmulInner { lhs, rhs } => write!(
                f,
                "matmul: inner dimensions differ for {lhs:?} @ {rhs:?}"
            ),
            NonScalarLoss { shape } => write!(
                f,
                "backward needs a single-element loss, got shape {shape:?}"
            ),
            BadCustomGradient { detail } => write!(f, "custom gradient: {detail}"),
            InvalidArgument { detail } => write!(f, "invalid argument: {detail}"),
        }
    }
}

impl std::error::Error for TensorError {}
