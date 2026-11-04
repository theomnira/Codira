//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Unit tests of the runtime's tensor services: creation and read-back,
//!   matmul, broadcasting add, relu and softmax against hand-computed
//!   values, the training step fitting `y = 2x + 1`, and an error path for
//!   every service (bad, freed and wrong-kind handles, shapes, broadcasts,
//!   inner dimensions, axes, buffer sizes, ranks, sizes and learning rates).

use super::*;

fn tensor(data: &[f32], shape: &[usize]) -> i64 {
    let handle = codira_ai_tensor_new(data.as_ptr() as usize, shape.as_ptr() as usize, shape.len());
    assert!(handle > 0, "tensor_new failed: {handle}");
    handle
}

fn read(handle: i64) -> Vec<f32> {
    let count = codira_ai_tensor_numel(handle);
    assert!(count >= 0, "numel failed: {count}");
    let mut out = vec![f32::NAN; count as usize];
    assert_eq!(
        codira_ai_tensor_read(handle, out.as_mut_ptr() as usize, out.len()),
        count
    );
    out
}

fn shape(handle: i64) -> Vec<usize> {
    let rank = codira_ai_tensor_rank(handle);
    assert!(rank >= 0);
    (0..rank as usize)
        .map(|axis| codira_ai_tensor_dim(handle, axis) as usize)
        .collect()
}

fn free(handles: &[i64]) {
    for &handle in handles {
        assert_eq!(codira_ai_tensor_free(handle), 0);
    }
}

#[test]
fn a_tensor_round_trips_with_its_shape() {
    let t = tensor(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &[2, 3]);
    assert_eq!(shape(t), [2, 3]);
    assert_eq!(codira_ai_tensor_numel(t), 6);
    assert_eq!(read(t), [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(codira_ai_tensor_dim(t, 2), i64::from(AI_ERR_OUT_OF_RANGE));

    // Rank 0 holds one element; a zero extent holds none and needs no
    // buffer.
    let scalar = tensor(&[7.5], &[]);
    assert_eq!(codira_ai_tensor_rank(scalar), 0);
    assert_eq!(read(scalar), [7.5]);
    let empty = codira_ai_tensor_new(0, [0usize, 4].as_ptr() as usize, 2);
    assert!(empty > 0);
    assert_eq!(codira_ai_tensor_numel(empty), 0);
    assert_eq!(codira_ai_tensor_read(empty, 0, 0), 0);
    free(&[t, scalar, empty]);
}

#[test]
fn unaligned_buffers_are_fine() {
    let mut bytes = [0u8; 1 + 4 * 4 + 1 + 8 * 2];
    for (i, value) in [1.0f32, -2.0, 3.5, 0.25].iter().enumerate() {
        bytes[1 + 4 * i..5 + 4 * i].copy_from_slice(&value.to_ne_bytes());
    }
    for (i, dim) in [2usize, 2].iter().enumerate() {
        bytes[18 + 8 * i..26 + 8 * i].copy_from_slice(&dim.to_ne_bytes());
    }
    let base = bytes.as_ptr() as usize;
    let t = codira_ai_tensor_new(base + 1, base + 18, 2);
    assert!(t > 0);
    assert_eq!(shape(t), [2, 2]);
    let mut out = vec![0u8; 1 + 16];
    assert_eq!(
        codira_ai_tensor_read(t, out.as_mut_ptr() as usize + 1, 4),
        4
    );
    assert_eq!(&out[1..], &bytes[1..17]);
    free(&[t]);
}

#[test]
fn matmul_add_relu_and_softmax_compute_the_expected_values() {
    let a = tensor(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], &[2, 3]);
    let b = tensor(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0], &[3, 2]);
    let c = codira_ai_matmul(a, b);
    assert!(c > 0);
    assert_eq!(shape(c), [2, 2]);
    // [1 2 3; 4 5 6] [7 8; 9 10; 11 12]
    assert_eq!(read(c), [58.0, 64.0, 139.0, 154.0]);
    // The operands are unchanged.
    assert_eq!(read(a), [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

    // Batched, with a broadcast batch axis: [2, 2, 3] @ [3, 2].
    let batch = tensor(
        &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0],
        &[2, 2, 3],
    );
    let bc = codira_ai_matmul(batch, b);
    assert_eq!(shape(bc), [2, 2, 2]);
    assert_eq!(read(bc), [7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 27.0, 30.0]);

    // Broadcasting add: [2, 2] + [2] adds the row to every row.
    let bias = tensor(&[-100.0, 0.5], &[2]);
    let sum = codira_ai_add(c, bias);
    assert_eq!(shape(sum), [2, 2]);
    assert_eq!(read(sum), [-42.0, 64.5, 39.0, 154.5]);

    let relu = codira_ai_relu(sum);
    assert_eq!(read(relu), [0.0, 64.5, 39.0, 154.5]);

    let logits = tensor(&[0.0, 0.0, 1.0, 1000.0, 1000.0, 1000.0], &[2, 3]);
    let soft = codira_ai_softmax(logits, 1);
    let p = read(soft);
    let e = std::f32::consts::E;
    let denom = 2.0 + e;
    let expected = [1.0 / denom, 1.0 / denom, e / denom];
    for (got, want) in p[..3].iter().zip(expected) {
        assert!((got - want).abs() < 1e-6, "{p:?}");
    }
    // Large logits do not overflow.
    for &got in &p[3..] {
        assert!((got - 1.0 / 3.0).abs() < 1e-6, "{p:?}");
    }
    // Along axis 0 every column sums to 1.
    let soft0 = codira_ai_softmax(logits, 0);
    let q = read(soft0);
    for column in 0..3 {
        assert!((q[column] + q[3 + column] - 1.0).abs() < 1e-6, "{q:?}");
    }
    free(&[a, b, c, batch, bc, bias, sum, relu, logits, soft, soft0]);
}

#[test]
fn shape_errors_have_their_own_codes() {
    let a = tensor(&[1.0; 6], &[2, 3]);
    let b = tensor(&[1.0; 6], &[2, 3]);
    let row = tensor(&[1.0; 3], &[3]);
    let wrong_row = tensor(&[1.0; 4], &[4]);
    assert_eq!(codira_ai_matmul(a, b), i64::from(AI_ERR_MATMUL_INNER));
    assert_eq!(codira_ai_matmul(a, row), i64::from(AI_ERR_SHAPE));
    assert_eq!(codira_ai_matmul(row, a), i64::from(AI_ERR_SHAPE));
    assert_eq!(codira_ai_add(a, wrong_row), i64::from(AI_ERR_BROADCAST));
    assert_eq!(codira_ai_softmax(a, 2), i64::from(AI_ERR_OUT_OF_RANGE));
    // Data whose count does not match the shape cannot be expressed: the
    // count is the shape's product. A result over the element limit is
    // refused before it is allocated: [2^15, 1] @ [1, 2^15] is 2^30.
    let tall = codira_ai_tensor_new(
        vec![0f32; 1 << 15].as_ptr() as usize,
        [1usize << 15, 1].as_ptr() as usize,
        2,
    );
    let wide = codira_ai_tensor_new(
        vec![0f32; 1 << 15].as_ptr() as usize,
        [1usize, 1 << 15].as_ptr() as usize,
        2,
    );
    assert!(tall > 0 && wide > 0);
    assert_eq!(codira_ai_matmul(tall, wide), i64::from(AI_ERR_TOO_LARGE));
    free(&[a, b, row, wrong_row, tall, wide]);
}

#[test]
fn arguments_are_validated_before_memory_is_read() {
    let invalid = i64::from(AI_ERR_INVALID_ARGUMENT);
    let data = [1.0f32; 4];
    let d = data.as_ptr() as usize;
    let nine = [1usize; 9];
    assert_eq!(codira_ai_tensor_new(d, nine.as_ptr() as usize, 9), invalid);
    assert_eq!(codira_ai_tensor_new(d, 0, 2), invalid);
    assert_eq!(
        codira_ai_tensor_new(0, [2usize, 2].as_ptr() as usize, 2),
        invalid
    );
    // Sizes are checked before the (bogus) data address is touched.
    let too_large = i64::from(AI_ERR_TOO_LARGE);
    let over = [AI_MAX_ELEMENTS + 1];
    assert_eq!(
        codira_ai_tensor_new(8, over.as_ptr() as usize, 1),
        too_large
    );
    let overflow = [usize::MAX, 2];
    assert_eq!(
        codira_ai_tensor_new(8, overflow.as_ptr() as usize, 2),
        too_large
    );

    let t = tensor(&data, &[4]);
    let mut out = [0xAAAA_AAAAu32; 3];
    assert_eq!(
        codira_ai_tensor_read(t, out.as_mut_ptr() as usize, 3),
        i64::from(AI_ERR_BUFFER_TOO_SMALL)
    );
    assert!(out.iter().all(|&word| word == 0xAAAA_AAAA));
    assert_eq!(codira_ai_tensor_read(t, 0, 4), invalid);
    free(&[t]);
}

#[test]
fn bad_freed_and_wrong_kind_handles_are_refused_by_every_operation() {
    let good = tensor(&[1.0; 4], &[2, 2]);
    let freed = tensor(&[1.0; 4], &[2, 2]);
    free(&[freed]);
    let layer = codira_ai_linear_new(2, 1, 1);
    assert!(layer > 0);
    let bad = i64::from(AI_ERR_BAD_HANDLE);
    let mut out = [0f32; 4];
    let o = out.as_mut_ptr() as usize;
    for handle in [freed, layer, 0, -1, i64::MIN, i64::MAX, good | (1 << 50), 1] {
        assert_eq!(codira_ai_tensor_rank(handle), bad, "{handle}");
        assert_eq!(codira_ai_tensor_dim(handle, 0), bad, "{handle}");
        assert_eq!(codira_ai_tensor_numel(handle), bad, "{handle}");
        assert_eq!(codira_ai_tensor_read(handle, o, 4), bad, "{handle}");
        assert_eq!(codira_ai_tensor_free(handle), AI_ERR_BAD_HANDLE);
        assert_eq!(codira_ai_matmul(handle, good), bad, "{handle}");
        assert_eq!(codira_ai_matmul(good, handle), bad, "{handle}");
        assert_eq!(codira_ai_add(handle, good), bad, "{handle}");
        assert_eq!(codira_ai_add(good, handle), bad, "{handle}");
        assert_eq!(codira_ai_relu(handle), bad, "{handle}");
        assert_eq!(codira_ai_softmax(handle, 0), bad, "{handle}");
        assert_eq!(codira_ai_linear_forward(layer, handle), bad, "{handle}");
        assert_eq!(
            codira_ai_linear_mse_sgd_step(layer, handle, good, 0.1, 0),
            AI_ERR_BAD_HANDLE
        );
        assert_eq!(
            codira_ai_linear_mse_sgd_step(layer, good, handle, 0.1, 0),
            AI_ERR_BAD_HANDLE
        );
    }
    for handle in [good, 0, -1, i64::MAX, layer | (1 << 50), 1] {
        assert_eq!(codira_ai_linear_forward(handle, good), bad, "{handle}");
        assert_eq!(codira_ai_linear_weight(handle), bad, "{handle}");
        assert_eq!(codira_ai_linear_bias(handle), bad, "{handle}");
        assert_eq!(
            codira_ai_linear_mse_sgd_step(handle, good, good, 0.1, 0),
            AI_ERR_BAD_HANDLE
        );
        assert_eq!(codira_ai_linear_free(handle), AI_ERR_BAD_HANDLE);
    }
    assert_eq!(codira_ai_linear_free(layer), 0);
    assert_eq!(codira_ai_linear_free(layer), AI_ERR_BAD_HANDLE);
    free(&[good]);
}

/// The points `x = -1, -0.75, ..., 1` and `y = 2x + 1`, as `[9, 1]`.
fn line_data() -> (i64, i64) {
    let xs: Vec<f32> = (0..9).map(|i| -1.0 + 0.25 * i as f32).collect();
    let ys: Vec<f32> = xs.iter().map(|x| 2.0 * x + 1.0).collect();
    (tensor(&xs, &[9, 1]), tensor(&ys, &[9, 1]))
}

#[test]
fn sgd_on_a_linear_layer_learns_y_equals_2x_plus_1() {
    let (x, y) = line_data();
    let layer = codira_ai_linear_new(1, 1, 42);
    assert!(layer > 0);
    let mut losses = Vec::new();
    for _ in 0..300 {
        let mut loss = f32::NAN;
        assert_eq!(
            codira_ai_linear_mse_sgd_step(layer, x, y, 0.2, &mut loss as *mut f32 as usize),
            0
        );
        losses.push(loss);
    }
    // Gradient descent on this convex quadratic with a step below 2/L never
    // increases the loss (checked until it reaches f32 rounding noise).
    for pair in losses.windows(2).filter(|pair| pair[0] > 1e-6) {
        assert!(pair[1] <= pair[0], "{losses:?}");
    }
    assert!(losses[0] > 0.1, "{losses:?}");
    assert!(*losses.last().unwrap() < 1e-6, "{losses:?}");

    let w = codira_ai_linear_weight(layer);
    let b = codira_ai_linear_bias(layer);
    assert_eq!(shape(w), [1, 1]);
    assert_eq!(shape(b), [1]);
    assert!((read(w)[0] - 2.0).abs() < 1e-3, "{:?}", read(w));
    assert!((read(b)[0] - 1.0).abs() < 1e-3, "{:?}", read(b));

    let probe = tensor(&[10.0], &[1, 1]);
    let prediction = codira_ai_linear_forward(layer, probe);
    assert!((read(prediction)[0] - 21.0).abs() < 1e-2);
    // A null loss address skips writing the loss.
    assert_eq!(codira_ai_linear_mse_sgd_step(layer, x, y, 0.2, 0), 0);
    free(&[x, y, w, b, probe, prediction]);
    assert_eq!(codira_ai_linear_free(layer), 0);
}

#[test]
fn layers_are_deterministic_per_seed_and_validate_their_arguments() {
    let first = codira_ai_linear_new(3, 2, 7);
    let second = codira_ai_linear_new(3, 2, 7);
    let other = codira_ai_linear_new(3, 2, 8);
    let (w1, w2, w3) = (
        codira_ai_linear_weight(first),
        codira_ai_linear_weight(second),
        codira_ai_linear_weight(other),
    );
    assert_eq!(shape(w1), [3, 2]);
    assert_eq!(read(w1), read(w2));
    assert_ne!(read(w1), read(w3));

    let invalid = i64::from(AI_ERR_INVALID_ARGUMENT);
    assert_eq!(codira_ai_linear_new(0, 2, 1), invalid);
    assert_eq!(codira_ai_linear_new(2, 0, 1), invalid);
    assert_eq!(
        codira_ai_linear_new(1 << 20, 1 << 20, 1),
        i64::from(AI_ERR_TOO_LARGE)
    );

    let x = tensor(&[1.0; 6], &[2, 3]);
    let narrow = tensor(&[1.0; 4], &[2, 2]);
    let flat = tensor(&[1.0; 3], &[3]);
    let y = tensor(&[1.0; 4], &[2, 2]);
    let bad_y = tensor(&[1.0; 3], &[3]);
    assert_eq!(
        codira_ai_linear_forward(first, narrow),
        i64::from(AI_ERR_SHAPE)
    );
    assert_eq!(
        codira_ai_linear_forward(first, flat),
        i64::from(AI_ERR_SHAPE)
    );
    for lr in [0.0, -0.1, f32::NAN, f32::INFINITY] {
        assert_eq!(
            codira_ai_linear_mse_sgd_step(first, x, y, lr, 0),
            AI_ERR_INVALID_ARGUMENT
        );
    }
    assert_eq!(
        codira_ai_linear_mse_sgd_step(first, narrow, y, 0.1, 0),
        AI_ERR_SHAPE
    );
    assert_eq!(
        codira_ai_linear_mse_sgd_step(first, x, bad_y, 0.1, 0),
        AI_ERR_BROADCAST
    );
    // None of the refused steps changed the layer.
    let after = codira_ai_linear_weight(first);
    assert_eq!(read(after), read(w2));
    let prediction = codira_ai_linear_forward(first, x);
    assert_eq!(shape(prediction), [2, 2]);

    free(&[w1, w2, w3, x, narrow, flat, y, bad_y, after, prediction]);
    for layer in [first, second, other] {
        assert_eq!(codira_ai_linear_free(layer), 0);
    }
}

#[test]
fn every_service_is_in_the_dispatch_table() {
    let names: Vec<String> = functions()
        .into_iter()
        .map(|definition| definition.prototype.name)
        .collect();
    assert_eq!(
        names,
        [
            "codira_ai_tensor_new",
            "codira_ai_tensor_rank",
            "codira_ai_tensor_dim",
            "codira_ai_tensor_numel",
            "codira_ai_tensor_read",
            "codira_ai_tensor_free",
            "codira_ai_matmul",
            "codira_ai_add",
            "codira_ai_relu",
            "codira_ai_softmax",
            "codira_ai_linear_new",
            "codira_ai_linear_forward",
            "codira_ai_linear_weight",
            "codira_ai_linear_bias",
            "codira_ai_linear_mse_sgd_step",
            "codira_ai_linear_free",
        ]
    );
}
