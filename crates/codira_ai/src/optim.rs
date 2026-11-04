//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`Sgd`] with momentum and (coupled) weight decay, in the PyTorch
//!   convention `v = mu v + (g + wd p)`, `p -= lr v`.
//! - [`Adam`] (Kingma and Ba, ICLR 2015) with bias correction, and its
//!   decoupled-weight-decay form AdamW (Loshchilov and Hutter, ICLR 2019).
//! - [`CosineWithWarmup`]: linear warmup, then cosine decay to a floor.
//! - [`clip_grad_norm`]: rescales all gradients together so their global
//!   L2 norm is at most a bound.

use crate::{
    error::{Result, TensorError},
    float::Float,
    nn::ParamStore,
    tensor::Tensor,
};

/// An update rule over all of a model's parameters.
pub trait Optimizer<T: Float> {
    /// Applies one update. `grads[i]` is the gradient of parameter `i`.
    fn step(&mut self, params: &mut ParamStore<T>, grads: &[Tensor<T>]) -> Result<()>;
    /// The current learning rate.
    fn lr(&self) -> f64;
    /// Sets the learning rate (for schedules).
    fn set_lr(&mut self, lr: f64);
}

fn check_grads<T: Float>(params: &ParamStore<T>, grads: &[Tensor<T>]) -> Result<()> {
    if grads.len() != params.len() {
        return Err(TensorError::InvalidArgument {
            detail: format!("{} gradients for {} parameters", grads.len(), params.len()),
        });
    }
    for (p, g) in params.values().iter().zip(grads) {
        if p.shape() != g.shape() {
            return Err(TensorError::ShapeMismatch {
                op: "optimizer step",
                expected: p.shape().to_vec(),
                got: g.shape().to_vec(),
            });
        }
    }
    Ok(())
}

fn zeros_like<T: Float>(params: &ParamStore<T>) -> Vec<Vec<T>> {
    params.values().iter().map(|p| vec![T::ZERO; p.numel()]).collect()
}

/// Stochastic gradient descent with optional momentum and weight decay.
#[derive(Clone, Debug)]
pub struct Sgd<T: Float> {
    pub lr: f64,
    pub momentum: f64,
    pub weight_decay: f64,
    velocity: Vec<Vec<T>>,
}

impl<T: Float> Sgd<T> {
    /// Plain SGD when `momentum = 0` and `weight_decay = 0`.
    pub fn new(lr: f64, momentum: f64, weight_decay: f64) -> Self {
        Sgd {
            lr,
            momentum,
            weight_decay,
            velocity: Vec::new(),
        }
    }
}

impl<T: Float> Optimizer<T> for Sgd<T> {
    fn step(&mut self, params: &mut ParamStore<T>, grads: &[Tensor<T>]) -> Result<()> {
        check_grads(params, grads)?;
        if self.velocity.is_empty() {
            self.velocity = zeros_like(params);
        }
        let (lr, mu, wd) = (T::from_f64(self.lr), T::from_f64(self.momentum), T::from_f64(self.weight_decay));
        for ((p, g), v) in params.values_mut().iter_mut().zip(grads).zip(&mut self.velocity) {
            let g = g.to_vec();
            let pd = p.data_mut();
            for i in 0..pd.len() {
                let gi = g[i] + wd * pd[i];
                v[i] = mu * v[i] + gi;
                pd[i] -= lr * v[i];
            }
        }
        Ok(())
    }
    fn lr(&self) -> f64 {
        self.lr
    }
    fn set_lr(&mut self, lr: f64) {
        self.lr = lr;
    }
}

/// Adam, or AdamW when `decoupled` is set.
#[derive(Clone, Debug)]
pub struct Adam<T: Float> {
    pub lr: f64,
    pub beta1: f64,
    pub beta2: f64,
    pub eps: f64,
    pub weight_decay: f64,
    /// `false`: L2 penalty added to the gradient (Adam). `true`: the
    /// parameter is shrunk by `lr * wd` directly (AdamW).
    pub decoupled: bool,
    m: Vec<Vec<T>>,
    v: Vec<Vec<T>>,
    t: u64,
}

impl<T: Float> Adam<T> {
    /// Adam with the usual defaults `beta = (0.9, 0.999)`, `eps = 1e-8`.
    pub fn adam(lr: f64) -> Self {
        Adam {
            lr,
            beta1: 0.9,
            beta2: 0.999,
            eps: 1e-8,
            weight_decay: 0.0,
            decoupled: false,
            m: Vec::new(),
            v: Vec::new(),
            t: 0,
        }
    }

    /// AdamW with decoupled weight decay `wd`.
    pub fn adamw(lr: f64, wd: f64) -> Self {
        Adam {
            weight_decay: wd,
            decoupled: true,
            ..Self::adam(lr)
        }
    }

    /// The number of steps taken.
    pub fn steps(&self) -> u64 {
        self.t
    }
}

impl<T: Float> Optimizer<T> for Adam<T> {
    fn step(&mut self, params: &mut ParamStore<T>, grads: &[Tensor<T>]) -> Result<()> {
        check_grads(params, grads)?;
        if self.m.is_empty() {
            self.m = zeros_like(params);
            self.v = zeros_like(params);
        }
        self.t += 1;
        let t = self.t as i32;
        let (b1, b2) = (self.beta1, self.beta2);
        let c1 = T::from_f64(1.0 - b1.powi(t));
        let c2 = T::from_f64(1.0 - b2.powi(t));
        let (lr, eps, wd) = (T::from_f64(self.lr), T::from_f64(self.eps), T::from_f64(self.weight_decay));
        let (b1, b2) = (T::from_f64(b1), T::from_f64(b2));
        for (((p, g), m), v) in params.values_mut().iter_mut().zip(grads).zip(&mut self.m).zip(&mut self.v) {
            let g = g.to_vec();
            let pd = p.data_mut();
            for i in 0..pd.len() {
                let mut gi = g[i];
                if self.decoupled {
                    pd[i] -= lr * wd * pd[i];
                } else {
                    gi += wd * pd[i];
                }
                m[i] = b1 * m[i] + (T::ONE - b1) * gi;
                v[i] = b2 * v[i] + (T::ONE - b2) * gi * gi;
                let mhat = m[i] / c1;
                let vhat = v[i] / c2;
                pd[i] -= lr * mhat / (vhat.sqrt() + eps);
            }
        }
        Ok(())
    }
    fn lr(&self) -> f64 {
        self.lr
    }
    fn set_lr(&mut self, lr: f64) {
        self.lr = lr;
    }
}

/// Linear warmup from `base_lr / warmup` to `base_lr` over `warmup` steps,
/// then cosine decay to `min_lr` at `total` steps, constant after.
#[derive(Clone, Copy, Debug)]
pub struct CosineWithWarmup {
    pub base_lr: f64,
    pub min_lr: f64,
    pub warmup: u64,
    pub total: u64,
}

impl CosineWithWarmup {
    /// The learning rate for step `step` (0-based).
    pub fn lr_at(&self, step: u64) -> f64 {
        if step < self.warmup {
            return self.base_lr * (step + 1) as f64 / self.warmup as f64;
        }
        let span = self.total.saturating_sub(self.warmup).max(1);
        let progress = ((step - self.warmup) as f64 / span as f64).min(1.0);
        self.min_lr + 0.5 * (self.base_lr - self.min_lr) * (1.0 + (std::f64::consts::PI * progress).cos())
    }
}

/// Scales every gradient by `max_norm / norm` when the global L2 norm
/// exceeds `max_norm`. Returns the norm before clipping.
pub fn clip_grad_norm<T: Float>(grads: &mut [Tensor<T>], max_norm: f64) -> f64 {
    let norm = grads
        .iter()
        .flat_map(|g| g.to_vec())
        .map(|x| {
            let x = x.to_f64();
            x * x
        })
        .sum::<f64>()
        .sqrt();
    if norm > max_norm {
        let s = T::from_f64(max_norm / norm);
        for g in grads.iter_mut() {
            *g = g.scale(s);
        }
    }
    norm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(x: &[f64]) -> ParamStore<f64> {
        let mut s = ParamStore::new();
        s.add("x", Tensor::from_vec(x.to_vec(), &[x.len()]).unwrap());
        s
    }

    fn g(x: &[f64]) -> Vec<Tensor<f64>> {
        vec![Tensor::from_vec(x.to_vec(), &[x.len()]).unwrap()]
    }

    #[test]
    fn sgd_momentum_matches_closed_form() {
        let mut s = store(&[1.0]);
        let mut opt = Sgd::new(0.1, 0.9, 0.0);
        opt.step(&mut s, &g(&[2.0])).unwrap(); // v = 2, x = 1 - 0.2
        opt.step(&mut s, &g(&[2.0])).unwrap(); // v = 3.8, x = 0.8 - 0.38
        assert!((s.values()[0].to_vec()[0] - 0.42).abs() < 1e-15);
    }

    #[test]
    fn adam_first_step_is_lr_times_sign() {
        let mut s = store(&[1.0, -1.0]);
        let mut opt = Adam::adam(0.01);
        opt.step(&mut s, &g(&[3.0, -0.002])).unwrap();
        let x = s.values()[0].to_vec();
        assert!((x[0] - 0.99).abs() < 1e-9);
        assert!((x[1] - (-0.99)).abs() < 1e-5);
        assert_eq!(opt.steps(), 1);
    }

    #[test]
    fn adamw_decay_is_decoupled() {
        // zero gradient: Adam with L2 still moves by ~lr (gradient = wd*x),
        // AdamW shrinks by exactly lr*wd*x and takes no adaptive step.
        let mut a = store(&[2.0]);
        let mut w = store(&[2.0]);
        let mut adam = Adam { weight_decay: 0.1, ..Adam::adam(0.01) };
        let mut adamw = Adam::adamw(0.01, 0.1);
        adam.step(&mut a, &g(&[0.0])).unwrap();
        adamw.step(&mut w, &g(&[0.0])).unwrap();
        assert!((a.values()[0].to_vec()[0] - 1.99).abs() < 1e-6);
        assert!((w.values()[0].to_vec()[0] - (2.0 - 0.01 * 0.1 * 2.0)).abs() < 1e-15);
    }

    #[test]
    fn optimisers_minimise_a_quadratic() {
        // f(x) = sum (x - c)^2 / 2, gradient x - c
        let c = [3.0, -1.0, 0.5];
        let opts: Vec<Box<dyn Optimizer<f64>>> = vec![
            Box::new(Sgd::new(0.1, 0.0, 0.0)),
            Box::new(Sgd::new(0.05, 0.9, 0.0)),
            Box::new(Adam::adam(0.1)),
            Box::new(Adam::adamw(0.1, 0.0)),
        ];
        for mut opt in opts {
            let mut s = store(&[0.0, 0.0, 0.0]);
            for _ in 0..500 {
                let x = s.values()[0].to_vec();
                let gr: Vec<f64> = x.iter().zip(c).map(|(a, b)| a - b).collect();
                opt.step(&mut s, &g(&gr)).unwrap();
            }
            let x = s.values()[0].to_vec();
            for (a, b) in x.iter().zip(c) {
                assert!((a - b).abs() < 1e-3, "{x:?}");
            }
        }
    }

    #[test]
    fn step_validates_gradients() {
        let mut s = store(&[1.0]);
        assert!(Sgd::new(0.1, 0.0, 0.0).step(&mut s, &[]).is_err());
        assert!(Adam::adam(0.1).step(&mut s, &g(&[1.0, 2.0])).is_err());
    }

    #[test]
    fn cosine_schedule_endpoints() {
        let s = CosineWithWarmup {
            base_lr: 1.0,
            min_lr: 0.1,
            warmup: 10,
            total: 110,
        };
        assert!((s.lr_at(0) - 0.1).abs() < 1e-15);
        assert!((s.lr_at(9) - 1.0).abs() < 1e-15);
        assert!((s.lr_at(10) - 1.0).abs() < 1e-15);
        assert!((s.lr_at(60) - 0.55).abs() < 1e-12);
        assert!((s.lr_at(110) - 0.1).abs() < 1e-15);
        assert!((s.lr_at(1000) - 0.1).abs() < 1e-15);
        for step in 10..110 {
            assert!(s.lr_at(step + 1) <= s.lr_at(step));
        }
    }

    #[test]
    fn clipping_bounds_the_global_norm() {
        let mut gs = vec![
            Tensor::<f64>::from_vec(vec![3.0], &[1]).unwrap(),
            Tensor::from_vec(vec![4.0], &[1]).unwrap(),
        ];
        assert_eq!(clip_grad_norm(&mut gs, 10.0), 5.0);
        assert_eq!(gs[0].to_vec(), vec![3.0]);
        assert_eq!(clip_grad_norm(&mut gs, 1.0), 5.0);
        let n: f64 = gs.iter().map(|g| g.to_vec()[0].powi(2)).sum::<f64>().sqrt();
        assert!((n - 1.0).abs() < 1e-15);
        assert!((gs[0].to_vec()[0] - 0.6).abs() < 1e-15);
    }
}
