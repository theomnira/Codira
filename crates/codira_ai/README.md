# codira_ai

Experimental.

`codira_ai` is the native tensor and automatic-differentiation engine that
the Codira runtime is to expose to `std.ai`. It is pure Rust and uses only
`std`: no BLAS, no array library, no Python, no external inference service.

**Codira programs reach a first slice of it**: f32 tensors, matmul,
broadcasting add, relu, softmax, and a Linear layer trained with an MSE + SGD
step, through the runtime services `codira_ai_*` (wrapped in
`std/ai/runtime.code`, conventions documented in
`crates/codira_runtime/src/native_ai.rs`).
Everything else below is tested from Rust only.

## Modules

| module | contents | tests |
|--------|----------|-------|
| `tensor` | `Tensor<T: Float>` over `f32`/`f64`: shared buffer + shape + strides + offset; zero-copy `permute`, `transpose`, `slice`, `broadcast_to`, `view`; `reshape` (copies only when it has to); NumPy broadcasting; element-wise ops; `sum`/`mean`/`max` along an axis; stable `softmax`/`log_softmax`; batched `matmul` (blocked) and `matmul_naive` (reference) | `tensor::*::tests` |
| `autograd` | tape-based reverse mode (`Graph`, `Var`, `Gradients`); rules for add, sub, mul, div, neg, scale, add_scalar, exp, ln, tanh, sigmoid, relu, sqrt, matmul, sum/mean (all and per axis), softmax, log_softmax, reshape, permute/transpose, `index_rows`, `pick`; `Graph::custom` (custom-gradient hook); `check::check_gradients` (central differences); `dual::Dual` (forward mode) | `tests/gradcheck.rs`, `autograd::*::tests` |
| `nn` | `ParamStore`/`Bound`; Xavier and Kaiming initialisers; `Linear`, `Embedding`, `LayerNorm`, `Mlp`; scaled dot-product attention with causal mask; `MultiHeadAttention`; pre-norm `TransformerBlock`; `cross_entropy`, `mse` | `nn::*::tests`, `tests/end_to_end.rs` |
| `optim` | `Sgd` (momentum, weight decay), `Adam`, AdamW (`Adam::adamw`), `CosineWithWarmup`, `clip_grad_norm` | `optim::tests` |
| `quant` | symmetric per-channel INT8 weights, per-row INT8 activations, INT8 matmul with exact `i32` accumulation, `evaluate` (measured error) | `quant::tests` |
| `tokenizer` | byte-level BPE: training, encoding, decoding | `tokenizer::tests` |

Errors are values (`TensorError`): illegal reshapes, broadcast mismatches,
bad axes, out-of-range ids, wrongly shaped custom gradients and
overflow-prone INT8 shapes return `Err`, not a panic.

## Testing

```sh
cargo test -p codira_ai                 # 78 tests: 65 unit, 7 gradcheck, 5 end-to-end, 1 doc
cargo test -p codira_ai --release       # the same, optimised (also passes)
cargo run  -p codira_ai --release --example bench
```

What the tests establish:

- **Every gradient rule** is compared with central differences in `f64`
  (`h = 1e-5`, scaled error `< 1e-6`) over several shapes, including six
  broadcasting pairs and batched matmul with broadcast batch axes. Each
  loss is `sum(op(x) * R)` with a fixed random `R`. Inputs come from a
  seeded xoshiro256** generator, so the tests are deterministic. `ln` and
  `sqrt` get inputs in `[0.5, 3)`; `relu` and divisors get `|x| >= 0.1`, to
  keep central differences off the kink and the pole.
- The checker itself catches a wrong rule
  (`autograd::check::tests::detects_a_wrong_custom_gradient`). Changing the
  softmax rule by 0.1% made `nn::attention::tests::sdpa_gradient_check_with_mask`
  fail. That mutation was tried by hand and is not part of the suite.
- Forward mode (`Dual`) and reverse mode agree on a JVP to `1e-12`
  (`forward_mode_agrees_with_reverse_mode`).
- A one-block causal transformer LM (token + position embeddings,
  pre-norm multi-head attention, ReLU MLP, linear head, cross-entropy) has
  all 301 of its parameters gradient-checked: max scaled error 1.5e-9
  (`tiny_causal_transformer_gradient_check`). It is causal: changing the
  last token leaves earlier logits bit-identical
  (`tiny_causal_transformer_is_causal`).
- Training works end to end. XOR (MLP 2-8-1, SGD momentum) reaches MSE
  below 1e-3: the f32 loss reaches exactly 0. Two interleaved spirals (120
  points, MLP 2-32-32-2, Adam, cosine warmup schedule, clipping) reach
  cross-entropy 0.013 and 120/120 accuracy. The transformer fits a
  7-token sequence, with loss falling from 4.20 to below 1e-4.

## Floating-point policy

- Nothing reassociates a sum without saying so. Reductions add in
  increasing index order. This is reproducible, but sequential summation
  has an error bound that grows linearly with length.
- The blocked matmul changes the loop order but **not** the summation
  order of any output element: `k` still increases, and accumulators carry
  across k-panels. Rust does not contract `a*b + c` into an FMA on its own.
  On the tested machine the blocked and naive kernels are therefore
  **bit-identical**, in debug and in release. The tests assert both
  bit-identity and a tolerance against an `f64` product
  (`4e-6 * k`). The tolerance is the property that has to survive when a
  future kernel deliberately reorders the sum (FMA, split SIMD
  accumulators, threads). At that point the bit-identity assertion must
  be removed and the change documented here.
- Results are not claimed to be bit-identical to any other library.
- `Dual` compares by real part, so piecewise functions differentiate along
  the branch taken.
- INT8 results differ from f32. `quant::evaluate` reports max and mean
  absolute error and the ratio to the analytic rounding bound. The INT8
  path is never claimed to be equivalent to f32.

## Measurements

Machine: 4 vCPUs (`nproc` = 4), `Intel(R) Xeon(R) Processor @ 2.10GHz`
(from `/proc/cpuinfo`; the flags include avx2, fma and avx512f), shared
cloud VM. Toolchain: rustc 1.97.1, `--release`, default target features
(x86-64 baseline, so the compiler emits SSE2 and no AVX). Single thread.
Each figure is the best of 3-5 repetitions. Runs on this VM vary: f64
256^3 measured 11.3 GFLOP/s on one run and 6.1 on the next.

| matmul (square, n^3) | blocked GFLOP/s | naive GFLOP/s | speedup | bit-identical |
|---|---|---|---|---|
| f32 64 | 24.3 | 2.7 | 9.1x | yes |
| f32 128 | 24.2 | 2.3 | 10.7x | yes |
| f32 256 | 23.7 | 2.1 | 11.3x | yes |
| f32 512 | 21.3 | 2.1 | 10.0x | yes |
| f32 1024 | 19.5 | 0.28 | 68.9x | yes |
| f64 256 | 6.1-11.3 | 2.2 | 2.8-5.4x | yes |
| f64 512 | 8.8 | 0.64 | 13.8x | yes |

| training step (forward + backward + optimiser), f32 | time |
|---|---|
| MLP 2-32-32-2, batch 200, Adam | 0.43 ms |
| one causal transformer block, d=64, 4 heads, batch 4 x 32 tokens, vocab 256 (83,008 parameters), AdamW | 8.7 ms |

| INT8, 64x512 @ 512x512, x ~ N(0,1), W ~ N(0, 0.05^2) | value |
|---|---|
| f32 blocked matmul | 1.55 ms |
| INT8 matmul including activation quantisation | 5.02 ms (**slower** than f32: the INT8 loop is scalar and not blocked) |
| output max / mean absolute error | 5.5e-2 / 9.6e-3 (max \|y\| = 5.08) |
| worst error as a fraction of the analytic bound | 0.091 |
| weight round-trip max absolute error | 8.3e-4 |

**Comparison with PyTorch, JAX, ONNX Runtime, llama.cpp or any BLAS: not
compared yet.** Nothing was run against them under the same conditions.
Treat these numbers as a baseline for this crate only.

## Not done

- No runtime C ABI binding, so `std.ai` cannot reach this crate.
- Single-threaded. No explicit SIMD, no packing of A and B panels, no FMA,
  no GPU.
- No graph compilation or kernel fusion. The tape runs eagerly, one
  closure per op.
- The INT8 kernel is a scalar reference loop. Activation quantisation is
  symmetric per row; there is no asymmetric (zero-point) variant here,
  although `std/ai/quantize.code` describes one.
- No convolution, pooling, dropout, batch norm or RMSNorm. There is no
  higher-order differentiation: rules are not themselves recorded on a
  tape.
- The tokenizer pre-splits only on ASCII whitespace and is not compatible
  with any published vocabulary. It has no serialisation format.
- Tensors are `f32`/`f64` (and `Dual`) only: no f16/bf16 and no integer
  tensors.
