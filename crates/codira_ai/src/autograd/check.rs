//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`check_gradients`]: compares the gradients [`Graph::backward`]
//!   computes with central differences `(f(x + h e_i) - f(x - h e_i)) / 2h`,
//!   one input element at a time, in `f64`.
//!
//! This is the oracle every gradient rule in the crate is tested against,
//! and it is public so that a user of [`Graph::custom`] can check a
//! hand-written rule the same way. Central differences have truncation
//! error `O(h^2)` and rounding error `O(eps / h)`; with `h = 1e-5` both
//! are far below the `1e-6` threshold the tests use for smooth functions.
//! Functions with kinks (`relu` at 0) are tested on inputs kept away from
//! the kink, which the test generators guarantee.

use super::{Graph, Var};
use crate::{
    error::{Result, TensorError},
    tensor::Tensor,
};

/// The worst disagreement found by [`check_gradients`].
#[derive(Clone, Debug)]
pub struct GradCheck {
    /// The largest `|analytic - numeric|`.
    pub max_abs_err: f64,
    /// The largest `|analytic - numeric| / max(1, |analytic|, |numeric|)`.
    pub max_scaled_err: f64,
    /// How many scalar partial derivatives were compared.
    pub checked: usize,
}

/// Checks the reverse-mode gradient of the scalar function `f` at `inputs`
/// against central differences with step `h`. `f` is called once on a
/// graph whose leaves are [`Graph::param`]s for the analytic gradient, and
/// twice per input element for the numeric one.
pub fn check_gradients<F>(f: F, inputs: &[Tensor<f64>], h: f64) -> Result<GradCheck>
where
    F: for<'g> Fn(&'g Graph<f64>, &[Var<'g, f64>]) -> Result<Var<'g, f64>>,
{
    let eval = |xs: &[Tensor<f64>]| -> Result<f64> {
        let g = Graph::new();
        let vars: Vec<_> = xs.iter().map(|x| g.constant(x.clone())).collect();
        f(&g, &vars)?.value().item()
    };

    let g = Graph::new();
    let vars: Vec<_> = inputs.iter().map(|x| g.param(x.clone())).collect();
    let loss = f(&g, &vars)?;
    if loss.value().numel() != 1 {
        return Err(TensorError::NonScalarLoss { shape: loss.shape() });
    }
    let grads = g.backward(loss)?;

    let mut report = GradCheck {
        max_abs_err: 0.0,
        max_scaled_err: 0.0,
        checked: 0,
    };
    let mut xs: Vec<Tensor<f64>> = inputs.iter().map(|t| t.contiguous()).collect();
    for (i, var) in vars.iter().enumerate() {
        let analytic = grads.wrt(*var).to_vec();
        for (j, &a) in analytic.iter().enumerate() {
            let orig = xs[i].to_vec()[j];
            xs[i].data_mut()[j] = orig + h;
            let fp = eval(&xs)?;
            xs[i].data_mut()[j] = orig - h;
            let fm = eval(&xs)?;
            xs[i].data_mut()[j] = orig;
            let numeric = (fp - fm) / (2.0 * h);
            let abs = (a - numeric).abs();
            let scaled = abs / 1f64.max(a.abs()).max(numeric.abs());
            report.max_abs_err = report.max_abs_err.max(abs);
            report.max_scaled_err = report.max_scaled_err.max(scaled);
            report.checked += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_a_wrong_custom_gradient() {
        let x = Tensor::from_vec(vec![0.5, -1.5], &[2]).unwrap();
        // d/dx sin-free cube: true gradient 3x^2, the rule claims 2x^2
        let bad = check_gradients(
            |g, v| {
                let val = v[0].value().map(|t| t * t * t);
                Ok(g.custom(&[v[0]], val, |gr, xs| gr.mul(&xs[0].map(|t| 2.0 * t * t)).map(|t| vec![t])).sum())
            },
            &[x.clone()],
            1e-5,
        )
        .unwrap();
        assert!(bad.max_scaled_err > 0.1);
        let good = check_gradients(
            |g, v| {
                let val = v[0].value().map(|t| t * t * t);
                Ok(g.custom(&[v[0]], val, |gr, xs| gr.mul(&xs[0].map(|t| 3.0 * t * t)).map(|t| vec![t])).sum())
            },
            &[x],
            1e-5,
        )
        .unwrap();
        assert!(good.max_scaled_err < 1e-8, "{good:?}");
        assert_eq!(good.checked, 2);
    }
}
