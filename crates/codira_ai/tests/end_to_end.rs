//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - End-to-end tests: an MLP trained to a loss threshold on two
//!   deterministic synthetic tasks (XOR, two interleaved spirals), and a
//!   tiny causal transformer whose whole forward/backward pass is
//!   gradient-checked, checked for causality, and trained to fit a
//!   sequence.

use codira_ai::{
    autograd::check::check_gradients,
    nn::{
        attention::TransformerBlock,
        init::Init,
        layers::{Activation, Embedding, Linear, Mlp},
        loss::{cross_entropy, mse},
        Bound, ParamStore,
    },
    optim::{clip_grad_norm, Adam, CosineWithWarmup, Optimizer, Sgd},
    Graph, Result, Rng, Tensor,
};

#[test]
fn mlp_learns_xor_with_sgd_momentum() {
    let mut rng = Rng::new(1);
    let mut store = ParamStore::<f32>::new();
    let mlp = Mlp::new(&mut store, "xor", &[2, 8, 1], Activation::Tanh, &mut rng);
    let x = Tensor::from_vec(vec![0., 0., 0., 1., 1., 0., 1., 1.], &[4, 2]).unwrap();
    let y = Tensor::from_vec(vec![0., 1., 1., 0.], &[4, 1]).unwrap();
    let mut opt = Sgd::new(0.1, 0.9, 0.0);
    let mut last = f32::INFINITY;
    for _ in 0..1000 {
        let g = Graph::new();
        let p = store.bind(&g);
        let loss = mse(
            mlp.forward(&p, g.constant(x.clone())).unwrap(),
            g.constant(y.clone()),
        )
        .unwrap();
        last = loss.value().item().unwrap();
        let grads = p.grads(&g.backward(loss).unwrap());
        opt.step(&mut store, &grads).unwrap();
    }
    let g = Graph::new();
    let p = store.bind(&g);
    let pred = mlp.forward(&p, g.constant(x)).unwrap().value().to_vec();
    eprintln!("xor: final mse {last:.3e}, predictions {pred:?}");
    assert!(last < 1e-3, "XOR loss {last}");
    for (p, t) in pred.iter().zip(y.to_vec()) {
        assert!((p - t).abs() < 0.05, "{pred:?}");
    }
}

/// Two interleaved spirals, `n` points per arm, with a little noise.
fn spirals(n: usize, rng: &mut Rng) -> (Tensor<f32>, Vec<usize>) {
    let mut xs = Vec::with_capacity(4 * n);
    let mut ys = Vec::with_capacity(2 * n);
    for class in 0..2 {
        for i in 0..n {
            let r = i as f64 / n as f64;
            let t = 1.75 * std::f64::consts::TAU * r + class as f64 * std::f64::consts::PI;
            xs.push((r * t.cos() + 0.02 * rng.normal()) as f32);
            xs.push((r * t.sin() + 0.02 * rng.normal()) as f32);
            ys.push(class);
        }
    }
    (Tensor::from_vec(xs, &[2 * n, 2]).unwrap(), ys)
}

#[test]
fn mlp_learns_two_spirals_with_adam_and_a_schedule() {
    let mut rng = Rng::new(7);
    let (x, y) = spirals(60, &mut rng);
    let mut store = ParamStore::<f32>::new();
    let mlp = Mlp::new(
        &mut store,
        "spiral",
        &[2, 32, 32, 2],
        Activation::Tanh,
        &mut rng,
    );
    let steps = 1000;
    let sched = CosineWithWarmup {
        base_lr: 0.02,
        min_lr: 0.001,
        warmup: 50,
        total: steps,
    };
    let mut opt = Adam::new(sched.base_lr);
    let mut loss_v = f32::INFINITY;
    for step in 0..steps {
        opt.set_lr(sched.lr_at(step));
        let g = Graph::new();
        let p = store.bind(&g);
        let logits = mlp.forward(&p, g.constant(x.clone())).unwrap();
        let loss = cross_entropy(logits, &y).unwrap();
        loss_v = loss.value().item().unwrap();
        let mut grads = p.grads(&g.backward(loss).unwrap());
        clip_grad_norm(&mut grads, 5.0);
        opt.step(&mut store, &grads).unwrap();
    }
    // accuracy of the trained model
    let g = Graph::new();
    let p = store.bind(&g);
    let logits = mlp.forward(&p, g.constant(x)).unwrap().value().to_vec();
    let correct = logits
        .chunks(2)
        .zip(&y)
        .filter(|(l, &c)| usize::from(l[1] > l[0]) == c)
        .count();
    eprintln!("spirals: final cross-entropy {loss_v:.4}, accuracy {correct}/120");
    assert!(loss_v < 0.05, "spiral loss {loss_v}");
    assert!(correct >= 119, "spiral accuracy {correct}/120");
}

/// A one-block causal language model: embedding + position embedding,
/// one transformer block, a linear head.
struct TinyLm {
    tok: Embedding,
    pos: Embedding,
    block: TransformerBlock,
    head: Linear,
    d: usize,
}

impl TinyLm {
    fn new(
        store: &mut ParamStore<f64>,
        vocab: usize,
        t: usize,
        d: usize,
        heads: usize,
        rng: &mut Rng,
    ) -> Self {
        TinyLm {
            tok: Embedding::new(store, "tok", vocab, d, rng),
            pos: Embedding::new(store, "pos", t, d, rng),
            block: TransformerBlock::new(store, "block", d, heads, rng).unwrap(),
            head: Linear::new(store, "head", d, vocab, Init::Xavier, rng),
            d,
        }
    }

    /// `ids: [b * t]` to logits `[b, t, vocab]`.
    fn forward<'g>(
        &self,
        p: &Bound<'g, f64>,
        ids: &[usize],
        b: usize,
        t: usize,
    ) -> Result<codira_ai::Var<'g, f64>> {
        let tok = self.tok.forward(p, ids)?.reshape(&[b, t, self.d])?;
        let positions: Vec<usize> = (0..t).collect();
        let pos = self.pos.forward(p, &positions)?; // [t, d], broadcast over b
        let h = self.block.forward(p, tok.add(pos)?, true)?;
        self.head.forward(p, h)
    }
}

fn lm_loss<'g>(
    lm: &TinyLm,
    p: &Bound<'g, f64>,
    ids: &[usize],
    targets: &[usize],
    b: usize,
    t: usize,
    vocab: usize,
) -> Result<codira_ai::Var<'g, f64>> {
    let logits = lm.forward(p, ids, b, t)?.reshape(&[b * t, vocab])?;
    cross_entropy(logits, targets)
}

#[test]
fn tiny_causal_transformer_gradient_check() {
    let (vocab, b, t, d, heads) = (5, 2, 3, 4, 2);
    let mut rng = Rng::new(11);
    let mut store = ParamStore::<f64>::new();
    let lm = TinyLm::new(&mut store, vocab, t, d, heads, &mut rng);
    // move LayerNorm gains and all biases off their trivial starting values
    let inputs: Vec<Tensor<f64>> = store
        .values()
        .iter()
        .map(|v| v.add(&Tensor::randn(v.shape(), 0.2, &mut rng)).unwrap())
        .collect();
    let ids = [1, 4, 2, 0, 3, 3];
    let targets = [4, 2, 0, 3, 3, 1];
    let rep = check_gradients(
        |_, v| {
            let p = Bound::from_vars(v.to_vec());
            lm_loss(&lm, &p, &ids, &targets, b, t, vocab)
        },
        &inputs,
        1e-5,
    )
    .unwrap();
    assert_eq!(rep.checked, store.numel());
    eprintln!("transformer gradcheck: {rep:?}");
    assert!(rep.max_scaled_err < 1e-6, "{rep:?}");
}

#[test]
fn tiny_causal_transformer_is_causal() {
    let (vocab, t, d, heads) = (6, 4, 8, 2);
    let mut rng = Rng::new(12);
    let mut store = ParamStore::<f64>::new();
    let lm = TinyLm::new(&mut store, vocab, t, d, heads, &mut rng);
    let run = |ids: &[usize]| {
        let g = Graph::new();
        let p = store.bind(&g);
        lm.forward(&p, ids, 1, t).unwrap().value().to_vec()
    };
    let a = run(&[1, 2, 3, 4]);
    let b = run(&[1, 2, 3, 0]); // only the last token differs
    let prefix = (t - 1) * vocab;
    assert_eq!(
        a[..prefix],
        b[..prefix],
        "earlier positions must not see the future"
    );
    assert_ne!(a[prefix..], b[prefix..]);
}

#[test]
fn tiny_causal_transformer_fits_a_sequence() {
    let (vocab, t, d, heads) = (7, 6, 16, 4);
    let mut rng = Rng::new(13);
    let mut store = ParamStore::<f64>::new();
    let lm = TinyLm::new(&mut store, vocab, t, d, heads, &mut rng);
    let seq = [0, 3, 1, 4, 1, 5, 2];
    let (ids, targets) = (&seq[..t], &seq[1..]);
    let mut opt = Adam::adamw(0.01, 0.01);
    let mut first = None;
    let mut last = 0.0;
    for _ in 0..200 {
        let g = Graph::new();
        let p = store.bind(&g);
        let loss = lm_loss(&lm, &p, ids, targets, 1, t, vocab).unwrap();
        last = loss.value().item().unwrap();
        first.get_or_insert(last);
        let grads = p.grads(&g.backward(loss).unwrap());
        opt.step(&mut store, &grads).unwrap();
    }
    let first = first.unwrap();
    eprintln!("sequence fit: loss {first:.4} -> {last:.4}");
    assert!(last < 0.05 && last < first / 20.0, "loss {first} -> {last}");
}
