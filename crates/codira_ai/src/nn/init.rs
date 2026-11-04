//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Weight initialisers. Xavier/Glorot (Glorot and Bengio, AISTATS 2010)
//!   keeps activation variance roughly constant for `tanh`-like layers;
//!   Kaiming/He (He et al., ICCV 2015) compensates for `ReLU` zeroing half
//!   its inputs. Both take an explicit [`Rng`], so a model initialised from
//!   a seed is reproducible.

use crate::{float::Float, rng::Rng, tensor::Tensor};

/// Which initialiser a layer uses for its weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Init {
    /// Uniform in `+-sqrt(6 / (fan_in + fan_out))`.
    Xavier,
    /// Normal with standard deviation `sqrt(2 / fan_in)`.
    Kaiming,
}

/// Uniform in `[-a, a)` with `a = sqrt(6 / (fan_in + fan_out))`.
pub fn xavier_uniform<T: Float>(
    shape: &[usize],
    fan_in: usize,
    fan_out: usize,
    rng: &mut Rng,
) -> Tensor<T> {
    let a = (6.0 / (fan_in + fan_out) as f64).sqrt();
    Tensor::rand_uniform(shape, -a, a, rng)
}

/// Normal with standard deviation `sqrt(2 / fan_in)`.
pub fn kaiming_normal<T: Float>(shape: &[usize], fan_in: usize, rng: &mut Rng) -> Tensor<T> {
    Tensor::randn(shape, (2.0 / fan_in as f64).sqrt(), rng)
}

impl Init {
    /// A weight of `shape` for a layer with the given fans.
    pub fn sample<T: Float>(
        self,
        shape: &[usize],
        fan_in: usize,
        fan_out: usize,
        rng: &mut Rng,
    ) -> Tensor<T> {
        match self {
            Init::Xavier => xavier_uniform(shape, fan_in, fan_out, rng),
            Init::Kaiming => kaiming_normal(shape, fan_in, rng),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xavier_bounds_and_variance() {
        let w: Tensor<f64> = xavier_uniform(&[200, 100], 200, 100, &mut Rng::new(1));
        let a = (6.0f64 / 300.0).sqrt();
        let v = w.to_vec();
        assert!(v.iter().all(|x| x.abs() <= a));
        // Var(U(-a, a)) = a^2 / 3 = 2 / (fan_in + fan_out)
        let var = v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64;
        assert!((var - 2.0 / 300.0).abs() < 0.05 * 2.0 / 300.0, "{var}");
    }

    #[test]
    fn kaiming_variance() {
        let w: Tensor<f64> = kaiming_normal(&[256, 128], 256, &mut Rng::new(2));
        let v = w.to_vec();
        let var = v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64;
        assert!((var - 2.0 / 256.0).abs() < 0.05 * 2.0 / 256.0, "{var}");
    }

    #[test]
    fn seeded_initialisation_is_reproducible() {
        let a: Tensor<f32> = Init::Kaiming.sample(&[3, 3], 3, 3, &mut Rng::new(9));
        let b: Tensor<f32> = Init::Kaiming.sample(&[3, 3], 3, 3, &mut Rng::new(9));
        assert_eq!(a, b);
    }
}
