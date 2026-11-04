//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Timing harness: matmul GFLOP/s for the blocked and naive kernels
//!   (f32 and f64, single thread), the time of one training step for two
//!   models, and INT8 matmul time and measured error.
//!
//! Run with `cargo run --release -p codira_ai --example bench`. Every
//! number is the best of several repetitions on whatever machine runs it;
//! nothing here is compared with another library.

use std::time::Instant;

use codira_ai::{
    nn::{
        attention::TransformerBlock,
        init::Init,
        layers::{Activation, Embedding, Linear, Mlp},
        loss::cross_entropy,
        ParamStore,
    },
    optim::{Adam, Optimizer},
    quant,
    tensor::matmul::{gemm_blocked, gemm_naive},
    Float, Graph, Rng, Tensor,
};

fn best_of<F: FnMut()>(reps: usize, mut f: F) -> f64 {
    (0..reps)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64()
        })
        .fold(f64::INFINITY, f64::min)
}

fn bench_gemm<T: Float>(name: &str, sizes: &[usize]) {
    let mut rng = Rng::new(1);
    for &n in sizes {
        let a: Vec<T> = (0..n * n).map(|_| T::from_f64(rng.normal())).collect();
        let b: Vec<T> = (0..n * n).map(|_| T::from_f64(rng.normal())).collect();
        let flops = 2.0 * (n * n * n) as f64;
        let reps = if n <= 256 { 5 } else { 3 };
        let mut c1 = vec![T::ZERO; n * n];
        let tb = best_of(reps, || {
            c1.fill(T::ZERO);
            gemm_blocked(n, n, n, &a, &b, &mut c1);
        });
        let mut c2 = vec![T::ZERO; n * n];
        let tn = best_of(reps, || {
            c2.fill(T::ZERO);
            gemm_naive(n, n, n, &a, &b, &mut c2);
        });
        let identical = c1 == c2;
        println!(
            "gemm {name} {n:>4}^3: blocked {:>6.2} GFLOP/s ({:>8.2} ms), naive {:>6.2} GFLOP/s ({:>8.2} ms), speedup {:>5.2}x, bit-identical: {identical}",
            flops / tb / 1e9,
            tb * 1e3,
            flops / tn / 1e9,
            tn * 1e3,
            tn / tb
        );
    }
}

fn bench_mlp_step() {
    let mut rng = Rng::new(2);
    let mut store = ParamStore::<f32>::new();
    let mlp = Mlp::new(&mut store, "m", &[2, 32, 32, 2], Activation::Tanh, &mut rng);
    let x = Tensor::<f32>::randn(&[200, 2], 1.0, &mut rng);
    let y: Vec<usize> = (0..200).map(|i| i % 2).collect();
    let mut opt = Adam::new(0.01);
    let mut step = || {
        let g = Graph::new();
        let p = store.bind(&g);
        let loss = cross_entropy(mlp.forward(&p, g.constant(x.clone())).unwrap(), &y).unwrap();
        let grads = p.grads(&g.backward(loss).unwrap());
        opt.step(&mut store, &grads).unwrap();
    };
    for _ in 0..10 {
        step();
    }
    let n = 200;
    let t = Instant::now();
    for _ in 0..n {
        step();
    }
    println!(
        "train step, MLP 2-32-32-2, batch 200, f32, forward+backward+Adam: {:.3} ms/step",
        t.elapsed().as_secs_f64() * 1e3 / f64::from(n)
    );
}

fn bench_transformer_step() {
    let (vocab, b, t, d, heads) = (256, 4, 32, 64, 4);
    let mut rng = Rng::new(3);
    let mut store = ParamStore::<f32>::new();
    let tok = Embedding::new(&mut store, "tok", vocab, d, &mut rng);
    let block = TransformerBlock::new(&mut store, "blk", d, heads, &mut rng).unwrap();
    let head = Linear::new(&mut store, "head", d, vocab, Init::Xavier, &mut rng);
    let ids: Vec<usize> = (0..b * t).map(|_| rng.below(vocab)).collect();
    let targets: Vec<usize> = (0..b * t).map(|_| rng.below(vocab)).collect();
    let mut opt = Adam::adamw(1e-3, 0.01);
    let mut step = || {
        let g = Graph::new();
        let p = store.bind(&g);
        let x = tok.forward(&p, &ids).unwrap().reshape(&[b, t, d]).unwrap();
        let h = block.forward(&p, x, true).unwrap();
        let logits = head
            .forward(&p, h)
            .unwrap()
            .reshape(&[b * t, vocab])
            .unwrap();
        let loss = cross_entropy(logits, &targets).unwrap();
        let grads = p.grads(&g.backward(loss).unwrap());
        opt.step(&mut store, &grads).unwrap();
    };
    step();
    let n = 20;
    let t0 = Instant::now();
    for _ in 0..n {
        step();
    }
    println!(
        "train step, 1 causal block d=64 h=4, batch 4 x 32 tokens, vocab 256, f32, {} params: {:.2} ms/step",
        store.numel(),
        t0.elapsed().as_secs_f64() * 1e3 / f64::from(n)
    );
}

fn bench_quant() {
    let mut rng = Rng::new(4);
    let (m, k, n) = (64, 512, 512);
    let x = Tensor::<f32>::randn(&[m, k], 1.0, &mut rng);
    let w = Tensor::<f32>::randn(&[k, n], 0.05, &mut rng);
    let qw = quant::quantize_per_channel(&w).unwrap();
    let tf = best_of(5, || {
        x.matmul(&w).unwrap();
    });
    let tq = best_of(5, || {
        quant::qmatmul(&x, &qw).unwrap();
    });
    let r = quant::evaluate(&x, &w).unwrap();
    println!(
        "int8 {m}x{k}x{n}: f32 blocked {:.2} ms, int8 (incl. activation quantisation) {:.2} ms; \
         max |err| {:.3e}, mean |err| {:.3e}, max |y| {:.3}, worst err/bound {:.3}, weight max |err| {:.3e}",
        tf * 1e3,
        tq * 1e3,
        r.output_max_abs_err,
        r.output_mean_abs_err,
        r.output_max_abs,
        r.worst_fraction_of_bound,
        r.weight_max_abs_err
    );
}

fn main() {
    bench_gemm::<f32>("f32", &[64, 128, 256, 512, 1024]);
    bench_gemm::<f64>("f64", &[256, 512]);
    bench_mlp_step();
    bench_transformer_step();
    bench_quant();
}
