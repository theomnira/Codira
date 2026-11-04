//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`cross_entropy`]: mean negative log-likelihood of integer class
//!   targets under `softmax(logits)`, computed through `log_softmax` so it
//!   stays finite for any finite logits.
//! - [`mse`]: mean squared error.

use crate::{autograd::Var, error::Result, float::Float};

/// `-(1/N) sum_n log softmax(logits)[n, targets[n]]`, `logits: [N, C]`.
pub fn cross_entropy<'g, T: Float>(logits: Var<'g, T>, targets: &[usize]) -> Result<Var<'g, T>> {
    Ok(logits.log_softmax(1)?.pick(targets)?.mean().neg())
}

/// `mean((pred - target)^2)`; shapes broadcast.
pub fn mse<'g, T: Float>(pred: Var<'g, T>, target: Var<'g, T>) -> Result<Var<'g, T>> {
    let d = pred.sub(target)?;
    Ok(d.mul(d)?.mean())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{autograd::check::check_gradients, Graph, Rng, Tensor};

    #[test]
    fn cross_entropy_values() {
        let g = Graph::<f64>::new();
        // uniform logits: loss = ln C
        let l = cross_entropy(g.constant(Tensor::zeros(&[3, 4])), &[0, 1, 3]).unwrap();
        assert!((l.value().item().unwrap() - 4f64.ln()).abs() < 1e-14);
        // huge logits stay finite
        let big = Tensor::from_vec(vec![1000.0, -1000.0], &[1, 2]).unwrap();
        let l = cross_entropy(g.constant(big), &[1])
            .unwrap()
            .value()
            .item()
            .unwrap();
        assert!((l - 2000.0).abs() < 1e-9);
        assert!(cross_entropy(g.constant(Tensor::zeros(&[2, 2])), &[0, 2]).is_err());
    }

    #[test]
    fn losses_gradient_check() {
        let mut rng = Rng::new(5);
        let logits = Tensor::randn(&[5, 3], 2.0, &mut rng);
        let rep = check_gradients(
            |_, v| cross_entropy(v[0], &[0, 2, 1, 1, 0]),
            &[logits],
            1e-5,
        )
        .unwrap();
        assert!(rep.max_scaled_err < 1e-6, "{rep:?}");
        let a = Tensor::randn(&[4, 2], 1.0, &mut rng);
        let b = Tensor::randn(&[4, 2], 1.0, &mut rng);
        let rep = check_gradients(|_, v| mse(v[0], v[1]), &[a, b], 1e-5).unwrap();
        assert!(rep.max_scaled_err < 1e-6, "{rep:?}");
    }
}
