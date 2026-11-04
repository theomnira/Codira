//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - The differentiable operations on [`Var`] and their gradient rules
//!   (vector-Jacobian products). Every rule here is checked against central
//!   differences in `tests/gradcheck.rs`.
//!
//! | op | forward | rule for upstream gradient `g` |
//! |----|---------|--------------------------------|
//! | `add` | `a + b` | `g`, `g` (each summed to its operand's shape) |
//! | `sub` | `a - b` | `g`, `-g` |
//! | `mul` | `a * b` | `g*b`, `g*a` |
//! | `div` | `a / b` | `g/b`, `-g*y/b` |
//! | `neg`, `scale(c)`, `add_scalar(c)` | | `-g`, `c*g`, `g` |
//! | `exp` | `y = e^a` | `g*y` |
//! | `ln` | `ln a` | `g/a` |
//! | `tanh` | `y = tanh a` | `g*(1-y^2)` |
//! | `sigmoid` | `y = s(a)` | `g*y*(1-y)` |
//! | `relu` | `max(a,0)` | `g*[a>0]` (subgradient 0 at 0) |
//! | `sqrt` | `y = sqrt a` | `g/(2y)` |
//! | `matmul` | `A@B` | `g@B^T`, `A^T@g` (summed over broadcast batch axes) |
//! | `sum`, `sum_axis` | | `g` broadcast back |
//! | `mean`, `mean_axis` | | `g/n` broadcast back |
//! | `softmax(ax)` | `y` | `y*(g - sum(g*y, ax))` |
//! | `log_softmax(ax)` | `y` | `g - e^y * sum(g, ax)` |
//! | `reshape`, `permute` | | inverse reshape / permutation |
//! | `index_rows(ids)` | `x[ids]` | scatter-add of `g` rows into zeros |
//! | `pick(t)` | `x[n, t_n]` | `g_n` at `(n, t_n)`, zero elsewhere |

use super::{Graph, Var};
use crate::{
    error::{Result, TensorError},
    float::Float,
    tensor::Tensor,
};

fn same_graph<T: Float>(a: &Var<'_, T>, b: &Var<'_, T>) -> Result<()> {
    if std::ptr::eq(a.graph, b.graph) {
        Ok(())
    } else {
        Err(TensorError::InvalidArgument {
            detail: "operands belong to different graphs".into(),
        })
    }
}

/// `g` with the axis `axis` re-inserted (extent 1) when it was dropped.
fn unsqueeze_if<T: Float>(g: &Tensor<T>, axis: usize, keepdim: bool) -> Result<Tensor<T>> {
    if keepdim {
        return Ok(g.clone());
    }
    let mut s = g.shape().to_vec();
    s.insert(axis, 1);
    g.reshape(&s)
}

impl<'g, T: Float> Var<'g, T> {
    fn unary(self, value: Tensor<T>, rule: impl Fn(&Tensor<T>) -> Result<Tensor<T>> + 'static) -> Var<'g, T> {
        self.graph.record(value, &[self], Box::new(move |g| Ok(vec![rule(g)?])))
    }

    // -------------------------------------------------------- element-wise

    /// `self + other`, broadcasting.
    pub fn add(self, other: Var<'g, T>) -> Result<Var<'g, T>> {
        same_graph(&self, &other)?;
        let (a, b) = (self.value(), other.value());
        let y = a.add(&b)?;
        let (sa, sb) = (a.shape().to_vec(), b.shape().to_vec());
        Ok(self.graph.record(
            y,
            &[self, other],
            Box::new(move |g| Ok(vec![g.reduce_to_shape(&sa)?, g.reduce_to_shape(&sb)?])),
        ))
    }

    /// `self - other`, broadcasting.
    pub fn sub(self, other: Var<'g, T>) -> Result<Var<'g, T>> {
        same_graph(&self, &other)?;
        let (a, b) = (self.value(), other.value());
        let y = a.sub(&b)?;
        let (sa, sb) = (a.shape().to_vec(), b.shape().to_vec());
        Ok(self.graph.record(
            y,
            &[self, other],
            Box::new(move |g| Ok(vec![g.reduce_to_shape(&sa)?, g.neg().reduce_to_shape(&sb)?])),
        ))
    }

    /// `self * other` element-wise, broadcasting.
    pub fn mul(self, other: Var<'g, T>) -> Result<Var<'g, T>> {
        same_graph(&self, &other)?;
        let (a, b) = (self.value(), other.value());
        let y = a.mul(&b)?;
        Ok(self.graph.record(
            y,
            &[self, other],
            Box::new(move |g| {
                Ok(vec![
                    g.mul(&b)?.reduce_to_shape(a.shape())?,
                    g.mul(&a)?.reduce_to_shape(b.shape())?,
                ])
            }),
        ))
    }

    /// `self / other` element-wise, broadcasting.
    pub fn div(self, other: Var<'g, T>) -> Result<Var<'g, T>> {
        same_graph(&self, &other)?;
        let (a, b) = (self.value(), other.value());
        let y = a.div(&b)?;
        let out = y.clone();
        let sa = a.shape().to_vec();
        Ok(self.graph.record(
            y,
            &[self, other],
            Box::new(move |g| {
                Ok(vec![
                    g.div(&b)?.reduce_to_shape(&sa)?,
                    g.mul(&out)?.div(&b)?.neg().reduce_to_shape(b.shape())?,
                ])
            }),
        ))
    }

    /// `-self`.
    pub fn neg(self) -> Var<'g, T> {
        self.unary(self.value().neg(), |g| Ok(g.neg()))
    }

    /// `c * self` for a constant `c`.
    pub fn scale(self, c: f64) -> Var<'g, T> {
        let c = T::from_f64(c);
        self.unary(self.value().scale(c), move |g| Ok(g.scale(c)))
    }

    /// `self + c` for a constant `c`.
    pub fn add_scalar(self, c: f64) -> Var<'g, T> {
        self.unary(self.value().add_scalar(T::from_f64(c)), |g| Ok(g.clone()))
    }

    /// `e^self`.
    pub fn exp(self) -> Var<'g, T> {
        let y = self.value().exp();
        let out = y.clone();
        self.unary(y, move |g| g.mul(&out))
    }

    /// Natural logarithm.
    pub fn ln(self) -> Var<'g, T> {
        let x = self.value();
        self.unary(x.ln(), move |g| g.div(&x))
    }

    /// Hyperbolic tangent.
    pub fn tanh(self) -> Var<'g, T> {
        let y = self.value().tanh();
        let d = y.map(|t| T::ONE - t * t);
        self.unary(y, move |g| g.mul(&d))
    }

    /// The logistic function.
    pub fn sigmoid(self) -> Var<'g, T> {
        let y = self.value().sigmoid();
        let d = y.map(|s| s * (T::ONE - s));
        self.unary(y, move |g| g.mul(&d))
    }

    /// `max(self, 0)`; the gradient at exactly 0 is taken to be 0.
    pub fn relu(self) -> Var<'g, T> {
        let x = self.value();
        let mask = x.map(|v| if v > T::ZERO { T::ONE } else { T::ZERO });
        self.unary(x.relu(), move |g| g.mul(&mask))
    }

    /// Square root.
    pub fn sqrt(self) -> Var<'g, T> {
        let y = self.value().sqrt();
        let d = y.map(|s| T::ONE / (s + s));
        self.unary(y, move |g| g.mul(&d))
    }

    // ------------------------------------------------------------- matmul

    /// `self @ other` (rank >= 2, batch axes broadcast).
    pub fn matmul(self, other: Var<'g, T>) -> Result<Var<'g, T>> {
        same_graph(&self, &other)?;
        let (a, b) = (self.value(), other.value());
        let y = a.matmul(&b)?;
        Ok(self.graph.record(
            y,
            &[self, other],
            Box::new(move |g| {
                let da = g.matmul(&b.t()?)?.reduce_to_shape(a.shape())?;
                let db = a.t()?.matmul(g)?.reduce_to_shape(b.shape())?;
                Ok(vec![da, db])
            }),
        ))
    }

    // --------------------------------------------------------- reductions

    /// The sum of every element (a rank-0 result).
    pub fn sum(self) -> Var<'g, T> {
        let x = self.value();
        let shape = x.shape().to_vec();
        self.unary(Tensor::scalar(x.sum_all()), move |g| Ok(g.broadcast_to(&shape)?.contiguous()))
    }

    /// The mean of every element (a rank-0 result).
    pub fn mean(self) -> Var<'g, T> {
        let n = self.value().numel();
        self.sum().scale(1.0 / n as f64)
    }

    /// Sum along `axis`.
    pub fn sum_axis(self, axis: usize, keepdim: bool) -> Result<Var<'g, T>> {
        let x = self.value();
        let y = x.sum_axis(axis, keepdim)?;
        let shape = x.shape().to_vec();
        Ok(self.unary(y, move |g| Ok(unsqueeze_if(g, axis, keepdim)?.broadcast_to(&shape)?.contiguous())))
    }

    /// Mean along `axis`.
    pub fn mean_axis(self, axis: usize, keepdim: bool) -> Result<Var<'g, T>> {
        let shape = self.shape();
        let n = *shape.get(axis).ok_or(TensorError::AxisOutOfRange {
            axis,
            rank: shape.len(),
        })?;
        Ok(self.sum_axis(axis, keepdim)?.scale(1.0 / n as f64))
    }

    // ------------------------------------------------------------ softmax

    /// Softmax along `axis` (stable; see [`Tensor::softmax`]).
    pub fn softmax(self, axis: usize) -> Result<Var<'g, T>> {
        let y = self.value().softmax(axis)?;
        let s = y.clone();
        Ok(self.unary(y, move |g| {
            let dot = g.mul(&s)?.sum_axis(axis, true)?;
            s.mul(&g.sub(&dot)?)
        }))
    }

    /// Log-softmax along `axis` (stable; see [`Tensor::log_softmax`]).
    pub fn log_softmax(self, axis: usize) -> Result<Var<'g, T>> {
        let y = self.value().log_softmax(axis)?;
        let p = y.exp();
        Ok(self.unary(y, move |g| g.sub(&p.mul(&g.sum_axis(axis, true)?)?)))
    }

    // -------------------------------------------------------------- shape

    /// Reshape (see [`Tensor::reshape`]).
    pub fn reshape(self, shape: &[usize]) -> Result<Var<'g, T>> {
        let x = self.value();
        let y = x.reshape(shape)?;
        let orig = x.shape().to_vec();
        Ok(self.unary(y, move |g| g.reshape(&orig)))
    }

    /// Axis permutation (see [`Tensor::permute`]).
    pub fn permute(self, perm: &[usize]) -> Result<Var<'g, T>> {
        let y = self.value().permute(perm)?;
        let mut inv = vec![0; perm.len()];
        for (i, &p) in perm.iter().enumerate() {
            inv[p] = i;
        }
        Ok(self.unary(y, move |g| g.permute(&inv)))
    }

    /// Swaps two axes.
    pub fn transpose(self, a: usize, b: usize) -> Result<Var<'g, T>> {
        let rank = self.shape().len();
        if a >= rank || b >= rank {
            return Err(TensorError::AxisOutOfRange {
                axis: a.max(b),
                rank,
            });
        }
        let mut perm: Vec<usize> = (0..rank).collect();
        perm.swap(a, b);
        self.permute(&perm)
    }

    // ------------------------------------------------------------ gathers

    /// Rows `ids` of a rank-2 `self` (an embedding lookup): `[V, D] -> [n, D]`.
    pub fn index_rows(self, ids: &[usize]) -> Result<Var<'g, T>> {
        let x = self.value();
        if x.rank() != 2 {
            return Err(TensorError::RankTooLow {
                op: "index_rows",
                needed: 2,
                got: x.rank(),
            });
        }
        let (v, d) = (x.shape()[0], x.shape()[1]);
        let src = x.contiguous();
        let src = src.as_slice().expect("contiguous");
        let mut out = Vec::with_capacity(ids.len() * d);
        for &id in ids {
            if id >= v {
                return Err(TensorError::IndexOutOfRange { index: id, bound: v });
            }
            out.extend_from_slice(&src[id * d..(id + 1) * d]);
        }
        let y = Tensor::from_vec(out, &[ids.len(), d])?;
        let ids = ids.to_vec();
        Ok(self.unary(y, move |g| {
            let g = g.contiguous();
            let gs = g.as_slice().expect("contiguous");
            let mut acc = Tensor::zeros(&[v, d]);
            let a = acc.data_mut();
            for (r, &id) in ids.iter().enumerate() {
                for c in 0..d {
                    a[id * d + c] += gs[r * d + c];
                }
            }
            Ok(acc)
        }))
    }

    /// `y[n] = self[n, targets[n]]` for a rank-2 `self` of shape `[N, C]`.
    pub fn pick(self, targets: &[usize]) -> Result<Var<'g, T>> {
        let x = self.value();
        if x.rank() != 2 || x.shape()[0] != targets.len() {
            return Err(TensorError::ShapeMismatch {
                op: "pick",
                expected: vec![targets.len(), x.shape().last().copied().unwrap_or(0)],
                got: x.shape().to_vec(),
            });
        }
        let (n, c) = (x.shape()[0], x.shape()[1]);
        let mut out = Vec::with_capacity(n);
        for (i, &t) in targets.iter().enumerate() {
            if t >= c {
                return Err(TensorError::IndexOutOfRange { index: t, bound: c });
            }
            out.push(x.get(&[i, t])?);
        }
        let y = Tensor::from_vec(out, &[n])?;
        let targets = targets.to_vec();
        Ok(self.unary(y, move |g| {
            let gv = g.to_vec();
            let mut acc = Tensor::zeros(&[n, c]);
            let a = acc.data_mut();
            for (i, &t) in targets.iter().enumerate() {
                a[i * c + t] += gv[i];
            }
            Ok(acc)
        }))
    }
}

impl<T: Float> Graph<T> {
    /// A constant scalar on this graph (convenience for losses and masks).
    pub fn scalar(&self, x: f64) -> Var<'_, T> {
        self.constant(Tensor::scalar(T::from_f64(x)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_on_different_graphs_are_rejected() {
        let g1 = Graph::<f64>::new();
        let g2 = Graph::<f64>::new();
        let a = g1.param(Tensor::ones(&[2]));
        let b = g2.param(Tensor::ones(&[2]));
        assert!(a.add(b).is_err());
    }

    #[test]
    fn gather_errors() {
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::ones(&[3, 2]));
        assert!(matches!(x.index_rows(&[3]), Err(TensorError::IndexOutOfRange { .. })));
        assert!(matches!(x.pick(&[0, 2, 1]), Err(TensorError::IndexOutOfRange { .. })));
        assert!(x.pick(&[0, 1]).is_err());
    }

    #[test]
    fn relu_subgradient_at_zero_is_zero() {
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::from_vec(vec![-1.0, 0.0, 2.0], &[3]).unwrap());
        let gr = g.backward(x.relu().sum()).unwrap();
        assert_eq!(gr.wrt(x).to_vec(), vec![0.0, 0.0, 1.0]);
    }
}
