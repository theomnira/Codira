//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Symmetric per-channel INT8 quantisation of a weight `W: [k, n]` (one
//!   scale per output column, `scale_j = max_i |W_ij| / 127`), the same
//!   symmetric scheme `std/ai/quantize.code` describes per tensor.
//! - Per-row symmetric INT8 quantisation of activations, computed at run
//!   time ("dynamic").
//! - [`qmatmul`]: `x @ W` with `i8 x i8` products accumulated in `i32` and
//!   one rescale per output, and [`evaluate`], which reports the measured
//!   error against the `f32` product.
//!
//! # What is and is not claimed
//!
//! Quantisation changes results. Nothing here claims equivalence with the
//! `f32` computation: [`evaluate`] measures the difference on the inputs it
//! is given, and the tests check it against the analytic worst-case bound
//! `sum_p (|x_ip| s_j / 2 + |W_pj| r_i / 2 + r_i s_j / 4)` for rounding to
//! nearest with row scale `r_i` and column scale `s_j`.
//!
//! The `i32` accumulator is exact as long as `k * 127 * 127 <= i32::MAX`,
//! i.e. `k <= 133_144`; [`qmatmul`] rejects larger `k` rather than
//! overflow.

use crate::{
    error::{Result, TensorError},
    tensor::Tensor,
};

/// The largest quantised magnitude. 127, not 128, so `-q` is representable
/// whenever `q` is and zero maps to exactly 0.
pub const QMAX: i32 = 127;

/// The largest inner dimension whose `i32` accumulation cannot overflow.
pub const MAX_K: usize = (i32::MAX / (QMAX * QMAX)) as usize;

/// A weight `[k, n]` stored as `i8` with one `f32` scale per column.
#[derive(Clone, Debug)]
pub struct QuantizedWeight {
    pub q: Vec<i8>,
    pub scales: Vec<f32>,
    pub k: usize,
    pub n: usize,
}

/// Activations `[m, k]` stored as `i8` with one `f32` scale per row.
#[derive(Clone, Debug)]
pub struct QuantizedRows {
    pub q: Vec<i8>,
    pub scales: Vec<f32>,
    pub m: usize,
    pub k: usize,
}

fn quantize_value(x: f32, scale: f32) -> i8 {
    if scale == 0.0 {
        return 0;
    }
    (x / scale).round().clamp(-(QMAX as f32), QMAX as f32) as i8
}

fn matrix_dims(t: &Tensor<f32>, op: &'static str) -> Result<(usize, usize)> {
    if t.rank() != 2 {
        return Err(TensorError::ShapeMismatch {
            op,
            expected: vec![0, 0],
            got: t.shape().to_vec(),
        });
    }
    Ok((t.shape()[0], t.shape()[1]))
}

/// Quantises each column of `w: [k, n]` with its own scale.
pub fn quantize_per_channel(w: &Tensor<f32>) -> Result<QuantizedWeight> {
    let (k, n) = matrix_dims(w, "quantize_per_channel")?;
    let data = w.to_vec();
    let mut scales = vec![0f32; n];
    for (j, s) in scales.iter_mut().enumerate() {
        let absmax = (0..k).map(|i| data[i * n + j].abs()).fold(0f32, f32::max);
        *s = absmax / QMAX as f32;
    }
    let q = data.iter().enumerate().map(|(idx, &x)| quantize_value(x, scales[idx % n])).collect();
    Ok(QuantizedWeight { q, scales, k, n })
}

impl QuantizedWeight {
    /// `q * scale` back in `f32`.
    pub fn dequantize(&self) -> Tensor<f32> {
        let data = self.q.iter().enumerate().map(|(i, &q)| q as f32 * self.scales[i % self.n]).collect();
        Tensor::from_vec(data, &[self.k, self.n]).expect("k * n elements")
    }
}

/// Quantises each row of `x: [m, k]` with its own scale.
pub fn quantize_rows(x: &Tensor<f32>) -> Result<QuantizedRows> {
    let (m, k) = matrix_dims(x, "quantize_rows")?;
    let data = x.to_vec();
    let mut q = Vec::with_capacity(m * k);
    let mut scales = Vec::with_capacity(m);
    for row in data.chunks(k.max(1)).take(m) {
        let s = row.iter().fold(0f32, |a, &b| a.max(b.abs())) / QMAX as f32;
        scales.push(s);
        q.extend(row.iter().map(|&v| quantize_value(v, s)));
    }
    Ok(QuantizedRows { q, scales, m, k })
}

/// The raw `i32` accumulators `sum_p xq[i,p] * wq[p,j]`.
pub fn int8_matmul_i32(x: &QuantizedRows, w: &QuantizedWeight) -> Result<Vec<i32>> {
    if x.k != w.k {
        return Err(TensorError::MatmulInner {
            lhs: vec![x.m, x.k],
            rhs: vec![w.k, w.n],
        });
    }
    if x.k > MAX_K {
        return Err(TensorError::InvalidArgument {
            detail: format!("inner dimension {} exceeds {MAX_K}, the i32 accumulator could overflow", x.k),
        });
    }
    let (m, k, n) = (x.m, x.k, w.n);
    let mut acc = vec![0i32; m * n];
    for i in 0..m {
        let row = &mut acc[i * n..(i + 1) * n];
        for p in 0..k {
            let a = x.q[i * k + p] as i32;
            if a == 0 {
                continue;
            }
            for (c, &b) in row.iter_mut().zip(&w.q[p * n..(p + 1) * n]) {
                *c += a * b as i32;
            }
        }
    }
    Ok(acc)
}

/// `x @ W` with `x` quantised per row at run time and `W` per column:
/// `y_ij = acc_ij * r_i * s_j`.
pub fn qmatmul(x: &Tensor<f32>, w: &QuantizedWeight) -> Result<Tensor<f32>> {
    let xq = quantize_rows(x)?;
    let acc = int8_matmul_i32(&xq, w)?;
    let n = w.n;
    let data = acc
        .iter()
        .enumerate()
        .map(|(idx, &a)| a as f32 * xq.scales[idx / n] * w.scales[idx % n])
        .collect();
    Tensor::from_vec(data, &[xq.m, n])
}

/// The measured difference between quantised and `f32` results.
#[derive(Clone, Debug)]
pub struct QuantReport {
    /// `max |W - dequant(quant(W))|`.
    pub weight_max_abs_err: f64,
    /// `max |x@W - qmatmul(x, W)|`.
    pub output_max_abs_err: f64,
    /// Mean of `|x@W - qmatmul(x, W)|`.
    pub output_mean_abs_err: f64,
    /// `max |x@W|`, to put the errors in proportion.
    pub output_max_abs: f64,
    /// The largest ratio of measured error to the analytic per-element bound
    /// (at most 1 if the bound holds; it is a bound, not an estimate).
    pub worst_fraction_of_bound: f64,
}

/// Quantises `w`, runs [`qmatmul`] on `x`, and measures it against the
/// blocked `f32` matmul.
pub fn evaluate(x: &Tensor<f32>, w: &Tensor<f32>) -> Result<QuantReport> {
    let qw = quantize_per_channel(w)?;
    let reference = x.matmul(w)?;
    let quant = qmatmul(x, &qw)?;
    let wmax = w.max_abs_diff(&qw.dequantize())?;
    let (m, k) = matrix_dims(x, "evaluate")?;
    let n = qw.n;
    let xr = quantize_rows(x)?;
    let (xv, wv) = (x.to_vec(), w.to_vec());
    let (rv, qv) = (reference.to_vec(), quant.to_vec());
    let (mut max_e, mut sum_e, mut max_ref, mut worst) = (0f64, 0f64, 0f64, 0f64);
    for i in 0..m {
        for j in 0..n {
            let e = (rv[i * n + j] as f64 - qv[i * n + j] as f64).abs();
            let (r, s) = (xr.scales[i] as f64, qw.scales[j] as f64);
            let bound: f64 = (0..k)
                .map(|p| (xv[i * k + p] as f64).abs() * s / 2.0 + (wv[p * n + j] as f64).abs() * r / 2.0 + r * s / 4.0)
                .sum();
            max_e = max_e.max(e);
            sum_e += e;
            max_ref = max_ref.max((rv[i * n + j] as f64).abs());
            if bound > 0.0 {
                worst = worst.max(e / bound);
            }
        }
    }
    Ok(QuantReport {
        weight_max_abs_err: wmax,
        output_max_abs_err: max_e,
        output_mean_abs_err: sum_e / (m * n).max(1) as f64,
        output_max_abs: max_ref,
        worst_fraction_of_bound: worst,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn per_channel_round_trip_is_within_half_a_step() {
        let mut rng = Rng::new(1);
        // columns with very different ranges: per-channel scales matter
        let w = Tensor::<f32>::from_fn(&[64, 8], |i| (rng.normal() * 10f64.powi((i % 8) as i32 - 4)) as f32);
        let q = quantize_per_channel(&w).unwrap();
        let d = q.dequantize();
        let (wv, dv) = (w.to_vec(), d.to_vec());
        for (i, (a, b)) in wv.iter().zip(&dv).enumerate() {
            let s = q.scales[i % 8];
            assert!((a - b).abs() <= s * 0.5 * (1.0 + 1e-5), "{a} {b} {s}");
        }
        assert!(q.q.iter().all(|&v| v != i8::MIN));
    }

    #[test]
    fn zero_channel_quantises_to_exact_zero() {
        let w = Tensor::<f32>::from_vec(vec![0.0, 1.0, 0.0, -2.0], &[2, 2]).unwrap();
        let q = quantize_per_channel(&w).unwrap();
        assert_eq!(q.scales[0], 0.0);
        let d = q.dequantize().to_vec();
        assert_eq!((d[0], d[2]), (0.0, 0.0));
        // the column's absmax is representable exactly; 1.0 is not (2/127 steps)
        assert_eq!(d[3], -2.0);
        assert!((d[1] - 1.0).abs() <= q.scales[1] / 2.0);
    }

    #[test]
    fn i32_accumulation_is_exact() {
        let mut rng = Rng::new(2);
        let x = Tensor::<f32>::randn(&[5, 300], 1.0, &mut rng);
        let w = Tensor::<f32>::randn(&[300, 7], 1.0, &mut rng);
        let (xq, wq) = (quantize_rows(&x).unwrap(), quantize_per_channel(&w).unwrap());
        let acc = int8_matmul_i32(&xq, &wq).unwrap();
        for i in 0..5 {
            for j in 0..7 {
                let exact: i64 = (0..300).map(|p| xq.q[i * 300 + p] as i64 * wq.q[p * 7 + j] as i64).sum();
                assert_eq!(acc[i * 7 + j] as i64, exact);
            }
        }
    }

    #[test]
    fn measured_error_respects_the_analytic_bound() {
        let mut rng = Rng::new(3);
        let x = Tensor::<f32>::randn(&[16, 128], 1.0, &mut rng);
        let w = Tensor::<f32>::randn(&[128, 32], 0.1, &mut rng);
        let r = evaluate(&x, &w).unwrap();
        assert!(r.worst_fraction_of_bound <= 1.0, "{r:?}");
        assert!(r.output_max_abs_err > 0.0, "quantisation is not lossless");
        assert!(r.output_mean_abs_err <= r.output_max_abs_err);
        // sanity: error is small relative to the outputs for Gaussian data
        assert!(r.output_max_abs_err < 0.05 * r.output_max_abs, "{r:?}");
    }

    #[test]
    fn rejects_bad_shapes_and_overflowing_k() {
        let x = quantize_rows(&Tensor::<f32>::ones(&[2, 3])).unwrap();
        let w = quantize_per_channel(&Tensor::<f32>::ones(&[4, 2])).unwrap();
        assert!(int8_matmul_i32(&x, &w).is_err());
        assert!(quantize_rows(&Tensor::<f32>::ones(&[3])).is_err());
        let big = QuantizedRows {
            q: vec![],
            scales: vec![],
            m: 0,
            k: MAX_K + 1,
        };
        let bigw = QuantizedWeight {
            q: vec![],
            scales: vec![],
            k: MAX_K + 1,
            n: 0,
        };
        assert!(matches!(int8_matmul_i32(&big, &bigw), Err(TensorError::InvalidArgument { .. })));
    }
}
