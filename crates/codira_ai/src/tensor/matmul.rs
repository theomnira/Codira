//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Matrix multiplication of rank >= 2 tensors with `NumPy` `matmul`
//!   semantics for the batch axes (all axes but the last two broadcast).
//! - [`gemm_blocked`]: a cache-blocked kernel with a 4x8 register tile.
//! - [`gemm_naive`] and [`Tensor::matmul_naive`]: the triple loop, kept as
//!   the test oracle and the baseline the benchmark compares against.
//!
//! # Floating-point policy
//!
//! The blocked kernel changes the *loop order* (which rows, columns and
//! k-panels are visited when) but not the *summation order* of any output
//! element: every `c[i][j]` is still `((0 + a[i][0]*b[0][j]) + a[i][1]*b[1][j]) + ...`
//! with `k` increasing, because the k-panels are visited in increasing
//! order and the accumulators carry over between panels. Rust does not
//! contract `a*b + c` into a fused multiply-add on its own. On the tested
//! configuration the two kernels are therefore bit-identical, and the tests
//! assert that; they also check a tolerance against an f64 reference,
//! which is the property that must survive if a later kernel (FMA, SIMD
//! with split accumulators, threads) deliberately reorders the sum.

use super::{shape::broadcast_shapes, Tensor};
use crate::{
    error::{Result, TensorError},
    float::Float,
};

/// Rows of A kept hot in L2 per block.
const MC: usize = 64;
/// Depth of one k-panel: MC x KC of A and KC x NC of B.
const KC: usize = 256;
/// Columns of B per block.
const NC: usize = 512;
/// Register tile: MR rows by NR columns of C.
const MR: usize = 4;
const NR: usize = 8;

/// `c[m x n] += a[m x k] * b[k x n]`, all row-major and contiguous, by the
/// textbook `i, j, k` loop with one accumulator per output element.
pub fn gemm_naive<T: Float>(m: usize, k: usize, n: usize, a: &[T], b: &[T], c: &mut [T]) {
    for i in 0..m {
        for j in 0..n {
            let mut acc = c[i * n + j];
            for p in 0..k {
                acc += a[i * k + p] * b[p * n + j];
            }
            c[i * n + j] = acc;
        }
    }
}

/// `c[m x n] += a[m x k] * b[k x n]`, all row-major and contiguous, cache
/// blocked. Summation order per element is the same as [`gemm_naive`]; see
/// the module documentation.
pub fn gemm_blocked<T: Float>(m: usize, k: usize, n: usize, a: &[T], b: &[T], c: &mut [T]) {
    assert!(a.len() >= m * k && b.len() >= k * n && c.len() >= m * n);
    for jc in (0..n).step_by(NC) {
        let nb = NC.min(n - jc);
        for pc in (0..k).step_by(KC) {
            let kb = KC.min(k - pc);
            for ic in (0..m).step_by(MC) {
                let mb = MC.min(m - ic);
                block(ic, mb, pc, kb, jc, nb, k, n, a, b, c);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn block<T: Float>(
    ic: usize,
    mb: usize,
    pc: usize,
    kb: usize,
    jc: usize,
    nb: usize,
    k: usize,
    n: usize,
    a: &[T],
    b: &[T],
    c: &mut [T],
) {
    let mut i = ic;
    while i < ic + mb {
        let rows = MR.min(ic + mb - i);
        let mut j = jc;
        while j < jc + nb {
            let cols = NR.min(jc + nb - j);
            if rows == MR && cols == NR {
                micro_4x8(i, j, pc, kb, k, n, a, b, c);
            } else {
                for r in i..i + rows {
                    for col in j..j + cols {
                        let mut acc = c[r * n + col];
                        for p in pc..pc + kb {
                            acc += a[r * k + p] * b[p * n + col];
                        }
                        c[r * n + col] = acc;
                    }
                }
            }
            j += NR;
        }
        i += MR;
    }
}

/// The register tile: a 4x8 block of C held in locals across one k-panel.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn micro_4x8<T: Float>(
    i: usize,
    j: usize,
    pc: usize,
    kb: usize,
    k: usize,
    n: usize,
    a: &[T],
    b: &[T],
    c: &mut [T],
) {
    let mut acc = [[T::ZERO; NR]; MR];
    for (r, row) in acc.iter_mut().enumerate() {
        row.copy_from_slice(&c[(i + r) * n + j..(i + r) * n + j + NR]);
    }
    for p in pc..pc + kb {
        let brow: &[T; NR] = b[p * n + j..p * n + j + NR].try_into().expect("NR wide");
        for (r, row) in acc.iter_mut().enumerate() {
            let av = a[(i + r) * k + p];
            for (cv, &bv) in row.iter_mut().zip(brow) {
                *cv += av * bv;
            }
        }
    }
    for (r, row) in acc.iter().enumerate() {
        c[(i + r) * n + j..(i + r) * n + j + NR].copy_from_slice(row);
    }
}

/// Shapes of a matmul: (batch shape, m, k, n).
fn matmul_dims(a: &[usize], b: &[usize]) -> Result<(Vec<usize>, usize, usize, usize)> {
    for (s, _) in [(a, 0), (b, 1)] {
        if s.len() < 2 {
            return Err(TensorError::RankTooLow {
                op: "matmul",
                needed: 2,
                got: s.len(),
            });
        }
    }
    let (m, k) = (a[a.len() - 2], a[a.len() - 1]);
    let (k2, n) = (b[b.len() - 2], b[b.len() - 1]);
    if k != k2 {
        return Err(TensorError::MatmulInner {
            lhs: a.to_vec(),
            rhs: b.to_vec(),
        });
    }
    let batch = broadcast_shapes("matmul", &a[..a.len() - 2], &b[..b.len() - 2])?;
    Ok((batch, m, k, n))
}

/// A kernel computing `c[m x n] += a[m x k] * b[k x n]`.
type Kernel<T> = fn(usize, usize, usize, &[T], &[T], &mut [T]);

impl<T: Float> Tensor<T> {
    fn matmul_with(&self, other: &Self, kernel: Kernel<T>) -> Result<Self> {
        let (batch, m, k, n) = matmul_dims(&self.shape, &other.shape)?;
        let mut a_shape = batch.clone();
        a_shape.extend([m, k]);
        let mut b_shape = batch.clone();
        b_shape.extend([k, n]);
        // Materialise the broadcast batch so each entry is one contiguous
        // matrix; for the common rank-2 case this is free.
        let a = self.broadcast_to(&a_shape)?.contiguous();
        let b = other.broadcast_to(&b_shape)?.contiguous();
        let (a, b) = (
            a.as_slice().expect("contiguous"),
            b.as_slice().expect("contiguous"),
        );
        let count: usize = batch.iter().product();
        let mut out = vec![T::ZERO; count * m * n];
        for e in 0..count {
            kernel(
                m,
                k,
                n,
                &a[e * m * k..(e + 1) * m * k],
                &b[e * k * n..(e + 1) * k * n],
                &mut out[e * m * n..(e + 1) * m * n],
            );
        }
        let mut shape = batch;
        shape.extend([m, n]);
        Tensor::from_vec(out, &shape)
    }

    /// `self @ other` with the cache-blocked kernel. Both operands need rank
    /// >= 2; batch axes broadcast.
    pub fn matmul(&self, other: &Self) -> Result<Self> {
        self.matmul_with(other, gemm_blocked)
    }

    /// `self @ other` by the naive triple loop: the reference oracle.
    pub fn matmul_naive(&self, other: &Self) -> Result<Self> {
        self.matmul_with(other, gemm_naive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn blocked_matches_naive_bit_for_bit_over_awkward_shapes() {
        let mut rng = Rng::new(11);
        for &(m, k, n) in &[
            (1, 1, 1),
            (3, 5, 7),
            (4, 8, 8),
            (17, 300, 9),
            (70, 513, 33),
            (5, 2, 600),
        ] {
            let a = Tensor::<f32>::randn(&[m, k], 1.0, &mut rng);
            let b = Tensor::<f32>::randn(&[k, n], 1.0, &mut rng);
            let fast = a.matmul(&b).unwrap();
            let slow = a.matmul_naive(&b).unwrap();
            assert_eq!(
                fast.to_vec(),
                slow.to_vec(),
                "bit-identical for {m}x{k}x{n}"
            );
            // and within a documented tolerance of the f64 product
            let exact = a.cast::<f64>().matmul_naive(&b.cast::<f64>()).unwrap();
            let tol = 1e-6 * k as f64 * 4.0; // ~ k * eps_f32 * typical |a||b|
            assert!(fast.cast::<f64>().max_abs_diff(&exact).unwrap() < tol);
        }
    }

    #[test]
    fn matmul_handles_views_and_batch_broadcast() {
        let mut rng = Rng::new(5);
        let a = Tensor::<f64>::randn(&[2, 1, 4, 3], 1.0, &mut rng);
        let b = Tensor::<f64>::randn(&[5, 4], 1.0, &mut rng).t().unwrap(); // [4,5] view
        let w = Tensor::<f64>::randn(&[3, 3, 5], 1.0, &mut rng);
        // [.., 4, 3] @ [4, 5]: inner 3 vs 4
        assert!(a.matmul(&b).is_err());
        // a transposed, sliced operand: [3, 4] view @ [4, 5] view
        let lhs = Tensor::<f64>::randn(&[4, 6], 1.0, &mut rng)
            .slice(1, 1, 4)
            .unwrap()
            .t()
            .unwrap();
        assert_eq!(
            lhs.matmul(&b).unwrap(),
            lhs.contiguous().matmul_naive(&b.contiguous()).unwrap()
        );
        let x = a.matmul(&w).unwrap();
        assert_eq!(x.shape(), &[2, 3, 4, 5]);
        // entry (1, 2) equals the plain 2-D product of its slices
        let a12 = a.slice(0, 1, 2).unwrap().reshape(&[4, 3]).unwrap();
        let w2 = w.slice(0, 2, 3).unwrap().reshape(&[3, 5]).unwrap();
        let x12 = x
            .slice(0, 1, 2)
            .unwrap()
            .slice(1, 2, 3)
            .unwrap()
            .reshape(&[4, 5])
            .unwrap();
        assert_eq!(x12, a12.matmul_naive(&w2).unwrap());
    }

    #[test]
    fn matmul_errors() {
        let a = Tensor::<f32>::zeros(&[2, 3]);
        assert!(matches!(
            a.matmul(&Tensor::zeros(&[4, 2])),
            Err(TensorError::MatmulInner { .. })
        ));
        assert!(matches!(
            a.matmul(&Tensor::zeros(&[3])),
            Err(TensorError::RankTooLow { .. })
        ));
        assert!(matches!(
            Tensor::<f32>::zeros(&[2, 2, 3]).matmul(&Tensor::zeros(&[3, 3, 1])),
            Err(TensorError::Broadcast { .. })
        ));
    }
}
