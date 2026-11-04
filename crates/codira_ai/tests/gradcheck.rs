//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Property tests: every differentiable op's reverse-mode gradient is
//!   compared with central differences (f64) over several shapes,
//!   including broadcasting, with inputs from a seeded generator. Each loss
//!   is `sum(op(x) * R)` for a fixed random `R`, so every output element
//!   contributes a different weight and a rule that gets one element wrong
//!   cannot cancel out.

use codira_ai::{
    autograd::{
        check::check_gradients,
        dual::{jvp, Dual},
        Graph, Var,
    },
    Result, Rng, Tensor,
};

const H: f64 = 1e-5;
const TOL: f64 = 1e-6;

/// `sum(y * R)` with `R` drawn from a fixed seed, so repeated evaluations
/// of the same function use the same weights.
fn weighted<'g>(g: &'g Graph<f64>, y: Var<'g, f64>) -> Result<Var<'g, f64>> {
    let r = Tensor::randn(&y.shape(), 1.0, &mut Rng::new(0xC0D1));
    Ok(y.mul(g.constant(r))?.sum())
}

fn assert_grad<F>(name: &str, f: F, inputs: &[Tensor<f64>])
where
    F: for<'g> Fn(&'g Graph<f64>, &[Var<'g, f64>]) -> Result<Var<'g, f64>>,
{
    let shapes: Vec<_> = inputs.iter().map(|t| t.shape().to_vec()).collect();
    let rep = check_gradients(f, inputs, H).unwrap_or_else(|e| panic!("{name} {shapes:?}: {e}"));
    assert!(rep.checked > 0 || inputs.iter().all(|t| t.numel() == 0));
    assert!(
        rep.max_scaled_err < TOL,
        "{name} {shapes:?}: scaled error {} (abs {})",
        rep.max_scaled_err,
        rep.max_abs_err
    );
}

fn normal(shape: &[usize], rng: &mut Rng) -> Tensor<f64> {
    Tensor::randn(shape, 1.0, rng)
}

/// Values in `[lo, hi)` (for `ln`, `sqrt`).
fn positive(shape: &[usize], rng: &mut Rng) -> Tensor<f64> {
    Tensor::rand_uniform(shape, 0.5, 3.0, rng)
}

/// Values with `0.1 <= |x| < 2`: away from 0, for `relu` and denominators.
fn away_from_zero(shape: &[usize], rng: &mut Rng) -> Tensor<f64> {
    Tensor::from_fn(shape, |_| {
        let m = rng.uniform(0.1, 2.0);
        if rng.next_u64() & 1 == 0 {
            m
        } else {
            -m
        }
    })
}

const BROADCAST_PAIRS: &[(&[usize], &[usize])] = &[
    (&[2, 3], &[2, 3]),
    (&[3, 4], &[4]),
    (&[2, 3, 4], &[3, 1]),
    (&[1, 5], &[4, 1]),
    (&[], &[3]),
    (&[2, 1, 3], &[2, 4, 1]),
];

const SHAPES: &[&[usize]] = &[&[5], &[2, 3], &[2, 3, 4], &[1, 4, 1]];

#[test]
fn binary_ops_with_broadcasting() {
    let mut rng = Rng::new(1);
    for &(sa, sb) in BROADCAST_PAIRS {
        let a = normal(sa, &mut rng);
        let b = normal(sb, &mut rng);
        let d = away_from_zero(sb, &mut rng);
        assert_grad(
            "add",
            |g, v| weighted(g, v[0].add(v[1])?),
            &[a.clone(), b.clone()],
        );
        assert_grad(
            "sub",
            |g, v| weighted(g, v[0].sub(v[1])?),
            &[a.clone(), b.clone()],
        );
        assert_grad(
            "mul",
            |g, v| weighted(g, v[0].mul(v[1])?),
            &[a.clone(), b.clone()],
        );
        assert_grad("div", |g, v| weighted(g, v[0].div(v[1])?), &[a.clone(), d]);
        // and with the operands swapped, so the broadcast side is the left one
        assert_grad(
            "mul swapped",
            |g, v| weighted(g, v[1].mul(v[0])?),
            &[a.clone(), b.clone()],
        );
    }
}

#[test]
fn unary_ops() {
    let mut rng = Rng::new(2);
    for &s in SHAPES {
        let x = normal(s, &mut rng);
        let p = positive(s, &mut rng);
        let z = away_from_zero(s, &mut rng);
        assert_grad(
            "neg",
            |g, v| weighted(g, v[0].neg()),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "scale",
            |g, v| weighted(g, v[0].scale(-2.5)),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "add_scalar",
            |g, v| weighted(g, v[0].add_scalar(0.7)),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "exp",
            |g, v| weighted(g, v[0].exp()),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "tanh",
            |g, v| weighted(g, v[0].tanh()),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "sigmoid",
            |g, v| weighted(g, v[0].sigmoid()),
            std::slice::from_ref(&x),
        );
        assert_grad(
            "ln",
            |g, v| weighted(g, v[0].ln()),
            std::slice::from_ref(&p),
        );
        assert_grad(
            "sqrt",
            |g, v| weighted(g, v[0].sqrt()),
            std::slice::from_ref(&p),
        );
        assert_grad(
            "relu",
            |g, v| weighted(g, v[0].relu()),
            std::slice::from_ref(&z),
        );
        // reuse of one input in two places
        assert_grad(
            "x*tanh(x)",
            |g, v| weighted(g, v[0].mul(v[0].tanh())?),
            std::slice::from_ref(&x),
        );
    }
}

#[test]
fn matmul_including_batch_broadcast_and_views() {
    let mut rng = Rng::new(3);
    let pairs: &[(&[usize], &[usize])] = &[
        (&[1, 1], &[1, 1]),
        (&[3, 4], &[4, 5]),
        (&[2, 3, 4], &[4, 2]),
        (&[3, 4], &[2, 4, 2]),
        (&[2, 1, 3, 4], &[3, 4, 2]),
    ];
    for &(sa, sb) in pairs {
        let a = normal(sa, &mut rng);
        let b = normal(sb, &mut rng);
        assert_grad("matmul", |g, v| weighted(g, v[0].matmul(v[1])?), &[a, b]);
    }
    // through a transposed view: x^T @ x
    let x = normal(&[4, 3], &mut rng);
    assert_grad(
        "xT@x",
        |g, v| weighted(g, v[0].transpose(0, 1)?.matmul(v[0])?),
        &[x],
    );
}

#[test]
fn reductions() {
    let mut rng = Rng::new(4);
    for &s in SHAPES {
        let x = normal(s, &mut rng);
        assert_grad("sum", |_, v| Ok(v[0].exp().sum()), std::slice::from_ref(&x));
        assert_grad(
            "mean",
            |_, v| Ok(v[0].tanh().mean()),
            std::slice::from_ref(&x),
        );
        for axis in 0..s.len() {
            for keepdim in [false, true] {
                assert_grad(
                    "sum_axis",
                    |g, v| weighted(g, v[0].sum_axis(axis, keepdim)?),
                    std::slice::from_ref(&x),
                );
                assert_grad(
                    "mean_axis",
                    |g, v| weighted(g, v[0].mean_axis(axis, keepdim)?),
                    std::slice::from_ref(&x),
                );
            }
        }
    }
}

#[test]
fn softmax_and_log_softmax_on_every_axis() {
    let mut rng = Rng::new(5);
    for &s in &[&[5][..], &[2, 3], &[2, 3, 4]] {
        let x = normal(s, &mut rng).scale(3.0);
        for axis in 0..s.len() {
            assert_grad(
                "softmax",
                |g, v| weighted(g, v[0].softmax(axis)?),
                std::slice::from_ref(&x),
            );
            assert_grad(
                "log_softmax",
                |g, v| weighted(g, v[0].log_softmax(axis)?),
                std::slice::from_ref(&x),
            );
        }
    }
    // large logits: the stable forms keep the gradient finite and right
    let big = normal(&[2, 4], &mut rng).scale(300.0);
    assert_grad(
        "log_softmax big",
        |g, v| weighted(g, v[0].log_softmax(1)?),
        &[big],
    );
}

#[test]
fn shape_ops_and_gathers() {
    let mut rng = Rng::new(6);
    let x = normal(&[2, 3, 4], &mut rng);
    assert_grad(
        "reshape",
        |g, v| weighted(g, v[0].reshape(&[6, 4])?.tanh()),
        std::slice::from_ref(&x),
    );
    assert_grad(
        "permute",
        |g, v| weighted(g, v[0].permute(&[2, 0, 1])?.tanh()),
        std::slice::from_ref(&x),
    );
    assert_grad(
        "permute then reshape (copying)",
        |g, v| weighted(g, v[0].permute(&[2, 0, 1])?.reshape(&[4, 6])?.sigmoid()),
        std::slice::from_ref(&x),
    );
    let table = normal(&[5, 3], &mut rng);
    assert_grad(
        "index_rows (repeated ids)",
        |g, v| weighted(g, v[0].index_rows(&[4, 0, 4, 2])?.tanh()),
        &[table],
    );
    let logits = normal(&[4, 3], &mut rng);
    assert_grad(
        "pick",
        |g, v| weighted(g, v[0].log_softmax(1)?.pick(&[2, 0, 1, 2])?),
        &[logits],
    );
}

#[test]
fn forward_mode_agrees_with_reverse_mode() {
    // For L(x) = sum(f(x) * R): the JVP gives sum(J v * R) = grad L . v.
    let mut rng = Rng::new(7);
    let x = normal(&[3, 4], &mut rng);
    let v = normal(&[3, 4], &mut rng);
    let r = Tensor::<f64>::randn(&[3, 4], 1.0, &mut rng);

    let f_dual = |t: &Tensor<Dual>| -> Result<Tensor<Dual>> { t.tanh().mul(&t.softmax(1)?) };
    let (y, jv) = jvp(f_dual, &x, &v).unwrap();
    let fwd = jv.mul(&r).unwrap().sum_all();

    let g = Graph::<f64>::new();
    let xv = g.param(x.clone());
    let out = xv.tanh().mul(xv.softmax(1).unwrap()).unwrap();
    assert!(out.value().allclose(&y, 1e-14, 1e-14));
    let loss = out.mul(g.constant(r)).unwrap().sum();
    let rev = g.backward(loss).unwrap().wrt(xv).mul(&v).unwrap().sum_all();
    assert!((fwd - rev).abs() < 1e-12, "{fwd} vs {rev}");
}
