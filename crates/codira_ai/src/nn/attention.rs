//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - [`scaled_dot_product_attention`]: `softmax(Q K^T / sqrt(d) + M) V`
//!   (Vaswani et al., NeurIPS 2017), with `M` the causal mask when asked.
//! - [`MultiHeadAttention`]: projections, head split, attention, merge.
//! - [`TransformerBlock`]: the pre-norm block
//!   `x + MHA(LN(x))`, then `h + MLP(LN(h))`.
//!
//! The causal mask adds `-inf` above the (offset) diagonal. Softmax
//! subtracts the row maximum first, and every row keeps at least its own
//! position, so masked entries become exactly 0 without any NaN, and
//! their gradient is exactly 0.

use super::{
    init::Init,
    layers::{Activation, LayerNorm, Linear, Mlp},
    Bound, ParamStore,
};
use crate::{
    autograd::Var,
    error::{Result, TensorError},
    float::Float,
    rng::Rng,
    tensor::Tensor,
};

/// The additive causal mask for `tq` queries over `tk` keys: query `i`
/// sits at absolute position `i + tk - tq` and may see keys `0..=` that.
pub fn causal_mask<T: Float>(tq: usize, tk: usize) -> Result<Tensor<T>> {
    if tq > tk {
        return Err(TensorError::InvalidArgument {
            detail: format!("causal attention with {tq} queries but only {tk} keys"),
        });
    }
    let off = tk - tq;
    Ok(Tensor::from_fn(&[tq, tk], |n| {
        let (i, j) = (n / tk, n % tk);
        if j <= i + off {
            T::ZERO
        } else {
            T::NEG_INFINITY
        }
    }))
}

/// `q: [..., tq, d]`, `k: [..., tk, d]`, `v: [..., tk, dv]` to `[..., tq, dv]`.
pub fn scaled_dot_product_attention<'g, T: Float>(q: Var<'g, T>, k: Var<'g, T>, v: Var<'g, T>, causal: bool) -> Result<Var<'g, T>> {
    let qs = q.shape();
    let ks = k.shape();
    if qs.len() < 2 || ks.len() < 2 {
        return Err(TensorError::RankTooLow {
            op: "attention",
            needed: 2,
            got: qs.len().min(ks.len()),
        });
    }
    let d = qs[qs.len() - 1];
    let rank = ks.len();
    let mut scores = q.matmul(k.transpose(rank - 2, rank - 1)?)?.scale(1.0 / (d as f64).sqrt());
    if causal {
        let mask = causal_mask(qs[qs.len() - 2], ks[ks.len() - 2])?;
        scores = scores.add(q.graph().constant(mask))?;
    }
    let last = scores.shape().len() - 1;
    scores.softmax(last)?.matmul(v)
}

/// Multi-head self-attention over `[batch, time, d_model]`.
#[derive(Clone, Debug)]
pub struct MultiHeadAttention {
    pub wq: Linear,
    pub wk: Linear,
    pub wv: Linear,
    pub wo: Linear,
    pub heads: usize,
    pub d_model: usize,
}

impl MultiHeadAttention {
    /// `d_model` must be a multiple of `heads`.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, d_model: usize, heads: usize, rng: &mut Rng) -> Result<Self> {
        if heads == 0 || d_model % heads != 0 {
            return Err(TensorError::InvalidArgument {
                detail: format!("d_model {d_model} is not a multiple of {heads} heads"),
            });
        }
        let mut lin = |s: &str| Linear::new(store, &format!("{name}.{s}"), d_model, d_model, Init::Xavier, rng);
        Ok(MultiHeadAttention {
            wq: lin("q"),
            wk: lin("k"),
            wv: lin("v"),
            wo: lin("o"),
            heads,
            d_model,
        })
    }

    /// `x: [b, t, d_model] -> [b, t, d_model]`.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, x: Var<'g, T>, causal: bool) -> Result<Var<'g, T>> {
        let s = x.shape();
        if s.len() != 3 || s[2] != self.d_model {
            return Err(TensorError::ShapeMismatch {
                op: "MultiHeadAttention",
                expected: vec![0, 0, self.d_model],
                got: s,
            });
        }
        let (b, t, h) = (s[0], s[1], self.heads);
        let dh = self.d_model / h;
        // [b, t, d] -> [b, h, t, dh]
        let split = |y: Var<'g, T>| y.reshape(&[b, t, h, dh])?.permute(&[0, 2, 1, 3]);
        let q = split(self.wq.forward(p, x)?)?;
        let k = split(self.wk.forward(p, x)?)?;
        let v = split(self.wv.forward(p, x)?)?;
        let o = scaled_dot_product_attention(q, k, v, causal)?;
        let merged = o.permute(&[0, 2, 1, 3])?.reshape(&[b, t, self.d_model])?;
        self.wo.forward(p, merged)
    }
}

/// A pre-norm transformer block with a two-layer ReLU MLP of width
/// `4 * d_model`.
#[derive(Clone, Debug)]
pub struct TransformerBlock {
    pub ln1: LayerNorm,
    pub attn: MultiHeadAttention,
    pub ln2: LayerNorm,
    pub mlp: Mlp,
}

impl TransformerBlock {
    /// A block for `d_model` with `heads` heads.
    pub fn new<T: Float>(store: &mut ParamStore<T>, name: &str, d_model: usize, heads: usize, rng: &mut Rng) -> Result<Self> {
        Ok(TransformerBlock {
            ln1: LayerNorm::new(store, &format!("{name}.ln1"), d_model, 1e-5),
            attn: MultiHeadAttention::new(store, &format!("{name}.attn"), d_model, heads, rng)?,
            ln2: LayerNorm::new(store, &format!("{name}.ln2"), d_model, 1e-5),
            mlp: Mlp::new(store, &format!("{name}.mlp"), &[d_model, 4 * d_model, d_model], Activation::Relu, rng),
        })
    }

    /// `x: [b, t, d_model] -> [b, t, d_model]`.
    pub fn forward<'g, T: Float>(&self, p: &Bound<'g, T>, x: Var<'g, T>, causal: bool) -> Result<Var<'g, T>> {
        let h = x.add(self.attn.forward(p, self.ln1.forward(p, x)?, causal)?)?;
        h.add(self.mlp.forward(p, self.ln2.forward(p, h)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{autograd::check::check_gradients, Graph};

    #[test]
    fn causal_mask_shape_and_offset() {
        let m = causal_mask::<f64>(2, 3).unwrap().to_vec();
        let inf = f64::NEG_INFINITY;
        assert_eq!(m, vec![0., 0., inf, 0., 0., 0.]);
        assert!(causal_mask::<f64>(3, 2).is_err());
    }

    #[test]
    fn attention_rows_are_convex_combinations_and_causal() {
        let mut rng = Rng::new(1);
        let g = Graph::<f64>::new();
        let q = g.constant(Tensor::randn(&[4, 3], 1.0, &mut rng));
        let k = g.constant(Tensor::randn(&[4, 3], 1.0, &mut rng));
        let v = g.constant(Tensor::from_fn(&[4, 1], |i| (i * 10) as f64));
        let o = scaled_dot_product_attention(q, k, v, true).unwrap().value().to_vec();
        assert_eq!(o[0], 0.0); // the first query sees only key 0
        for (i, &x) in o.iter().enumerate() {
            assert!((0.0..=(i * 10) as f64 + 1e-12).contains(&x));
        }
    }

    #[test]
    fn sdpa_gradient_check_with_mask() {
        let mut rng = Rng::new(2);
        let inputs = vec![
            Tensor::randn(&[2, 3, 4], 1.0, &mut rng),
            Tensor::randn(&[2, 3, 4], 1.0, &mut rng),
            Tensor::randn(&[2, 3, 2], 1.0, &mut rng),
        ];
        let r = Tensor::<f64>::randn(&[2, 3, 2], 1.0, &mut rng);
        for causal in [false, true] {
            let rep = check_gradients(
                |g, v| Ok(scaled_dot_product_attention(v[0], v[1], v[2], causal)?.mul(g.constant(r.clone()))?.sum()),
                &inputs,
                1e-5,
            )
            .unwrap();
            assert!(rep.max_scaled_err < 1e-6, "causal={causal}: {rep:?}");
        }
    }

    #[test]
    fn mha_rejects_bad_heads() {
        let mut s = ParamStore::<f64>::new();
        assert!(MultiHeadAttention::new(&mut s, "a", 6, 4, &mut Rng::new(0)).is_err());
    }
}
