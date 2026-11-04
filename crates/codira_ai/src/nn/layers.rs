//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `Linear` (`y = x W + b`, `W` stored `[in, out]` as in
//!   `std/ai/nn/layer.code`), `Embedding`, `LayerNorm` over the last axis,
//!   and `Mlp`, all built from the differentiable ops in `autograd::ops`,
//!   so their gradients follow from rules that are individually checked.

use super::{init::Init, Bound, ParamId, ParamStore};
use crate::{autograd::Var, error::Result, float::Float, rng::Rng, tensor::Tensor, TensorError};

/// `y = x W + b` over the last axis of `x`.
#[derive(Clone, Debug)]
pub struct Linear {
    pub w: ParamId,
    pub b: Option<ParamId>,
    pub in_dim: usize,
    pub out_dim: usize,
}

impl Linear {
    /// A layer with a bias initialised to zero.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, in_dim: usize, out_dim: usize, init: Init, rng: &mut Rng) -> Self {
        let w = store.add(format!("{name}.w"), init.sample(&[in_dim, out_dim], in_dim, out_dim, rng));
        let b = Some(store.add(format!("{name}.b"), Tensor::zeros(&[out_dim])));
        Linear { w, b, in_dim, out_dim }
    }

    /// A layer without a bias.
    pub fn new_no_bias<T: Float>(store: &mut ParamStore<T>, name: &str, in_dim: usize, out_dim: usize, init: Init, rng: &mut Rng) -> Self {
        let w = store.add(format!("{name}.w"), init.sample(&[in_dim, out_dim], in_dim, out_dim, rng));
        Linear { w, b: None, in_dim, out_dim }
    }

    /// `x: [..., in] -> [..., out]`.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, x: Var<'g, T>) -> Result<Var<'g, T>> {
        let y = x.matmul(p.var(self.w))?;
        match self.b {
            Some(b) => y.add(p.var(b)),
            None => Ok(y),
        }
    }
}

/// A lookup table of `vocab` vectors of width `dim`.
#[derive(Clone, Debug)]
pub struct Embedding {
    pub table: ParamId,
    pub vocab: usize,
    pub dim: usize,
}

impl Embedding {
    /// Entries drawn from `N(0, 1)`.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, vocab: usize, dim: usize, rng: &mut Rng) -> Self {
        let table = store.add(format!("{name}.table"), Tensor::randn(&[vocab, dim], 1.0, rng));
        Embedding { table, vocab, dim }
    }

    /// `ids: n -> [n, dim]`; an id `>= vocab` is an error.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, ids: &[usize]) -> Result<Var<'g, T>> {
        p.var(self.table).index_rows(ids)
    }
}

/// Layer normalisation over the last axis (Ba, Kiros and Hinton, 2016):
/// `(x - mean) / sqrt(var + eps) * gamma + beta`, with the biased variance.
#[derive(Clone, Debug)]
pub struct LayerNorm {
    pub gamma: ParamId,
    pub beta: ParamId,
    pub dim: usize,
    pub eps: f64,
}

impl LayerNorm {
    /// `gamma = 1`, `beta = 0`.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, dim: usize, eps: f64) -> Self {
        let gamma = store.add(format!("{name}.gamma"), Tensor::ones(&[dim]));
        let beta = store.add(format!("{name}.beta"), Tensor::zeros(&[dim]));
        LayerNorm { gamma, beta, dim, eps }
    }

    /// `x: [..., dim] -> [..., dim]`.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, x: Var<'g, T>) -> Result<Var<'g, T>> {
        let shape = x.shape();
        if shape.last() != Some(&self.dim) {
            return Err(TensorError::ShapeMismatch {
                op: "LayerNorm",
                expected: vec![self.dim],
                got: shape,
            });
        }
        let ax = shape.len() - 1;
        let mu = x.mean_axis(ax, true)?;
        let xc = x.sub(mu)?;
        let var = xc.mul(xc)?.mean_axis(ax, true)?;
        let xhat = xc.div(var.add_scalar(self.eps).sqrt())?;
        xhat.mul(p.var(self.gamma))?.add(p.var(self.beta))
    }
}

/// The nonlinearity between `Mlp` layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Tanh,
    Relu,
}

impl Activation {
    /// Applies the activation.
    pub fn apply<T: Float>(self, x: Var<'_, T>) -> Var<'_, T> {
        match self {
            Activation::Tanh => x.tanh(),
            Activation::Relu => x.relu(),
        }
    }
}

/// A stack of `Linear` layers with an activation between them (none after
/// the last).
#[derive(Clone, Debug)]
pub struct Mlp {
    pub layers: Vec<Linear>,
    pub activation: Activation,
}

impl Mlp {
    /// `dims = [in, hidden.., out]`. Weights use Kaiming for ReLU and
    /// Xavier for tanh.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, dims: &[usize], activation: Activation, rng: &mut Rng) -> Self {
        let init = match activation {
            Activation::Relu => Init::Kaiming,
            Activation::Tanh => Init::Xavier,
        };
        let layers = dims
            .windows(2)
            .enumerate()
            .map(|(i, w)| Linear::new(store, &format!("{name}.{i}"), w[0], w[1], init, rng))
            .collect();
        Mlp { layers, activation }
    }

    /// `x: [..., in] -> [..., out]`.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, mut x: Var<'g, T>) -> Result<Var<'g, T>> {
        for (i, layer) in self.layers.iter().enumerate() {
            x = layer.forward(p, x)?;
            if i + 1 < self.layers.len() {
                x = self.activation.apply(x);
            }
        }
        Ok(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{autograd::check::check_gradients, Graph};

    #[test]
    fn layer_norm_normalises_rows() {
        let mut store = ParamStore::<f64>::new();
        let ln = LayerNorm::new(&mut store, "ln", 4, 1e-5);
        let g = Graph::new();
        let p = store.bind(&g);
        let x = g.constant(Tensor::randn(&[3, 4], 5.0, &mut Rng::new(1)).add_scalar(7.0));
        let y = ln.forward(&p, x).unwrap().value();
        for row in y.to_vec().chunks(4) {
            let m: f64 = row.iter().sum::<f64>() / 4.0;
            let v: f64 = row.iter().map(|r| (r - m) * (r - m)).sum::<f64>() / 4.0;
            assert!(m.abs() < 1e-12);
            assert!((v - 1.0).abs() < 1e-5);
        }
        let bad = g.constant(Tensor::ones(&[3, 5]));
        assert!(ln.forward(&p, bad).is_err());
    }

    #[test]
    fn layers_gradient_check() {
        let mut rng = Rng::new(3);
        let mut store = ParamStore::<f64>::new();
        let mlp = Mlp::new(&mut store, "mlp", &[3, 5, 2], Activation::Tanh, &mut rng);
        let ln = LayerNorm::new(&mut store, "ln", 2, 1e-5);
        let emb = Embedding::new(&mut store, "emb", 6, 3, &mut rng);
        // perturb gamma/beta/biases away from their trivial initial values
        let mut inputs: Vec<Tensor<f64>> = store.values().iter().map(|t| t.add(&Tensor::randn(t.shape(), 0.3, &mut rng)).unwrap()).collect();
        let n = inputs.len();
        inputs.push(Tensor::randn(&[2, 4, 3], 1.0, &mut rng));
        let r = Tensor::<f64>::randn(&[2, 4, 2], 1.0, &mut rng);
        let rep = check_gradients(
            |g, v| {
                let p = Bound::from_vars(v[..n].to_vec());
                let tok = emb.forward(&p, &[1, 5, 5, 0, 2, 3, 4, 1])?.reshape(&[2, 4, 3])?;
                let x = v[n].add(tok)?;
                let y = ln.forward(&p, mlp.forward(&p, x)?)?;
                Ok(y.mul(g.constant(r.clone()))?.sum())
            },
            &inputs,
            1e-5,
        )
        .unwrap();
        assert!(rep.max_scaled_err < 1e-6, "{rep:?}");
    }

    #[test]
    fn relu_mlp_and_no_bias_linear_shapes() {
        let mut rng = Rng::new(4);
        let mut store = ParamStore::<f32>::new();
        let mlp = Mlp::new(&mut store, "m", &[4, 8, 8, 3], Activation::Relu, &mut rng);
        let lin = Linear::new_no_bias(&mut store, "l", 3, 2, Init::Xavier, &mut rng);
        assert_eq!(store.len(), 7);
        assert_eq!(store.name(lin.w), "l.w");
        let g = Graph::new();
        let p = store.bind(&g);
        let x = g.constant(Tensor::ones(&[5, 4]));
        assert_eq!(lin.forward(&p, mlp.forward(&p, x).unwrap()).unwrap().shape(), vec![5, 2]);
    }
}
