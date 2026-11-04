//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Tensor services the runtime provides to Codira programs through
//!   `extern "C"` declarations (`std/ai/runtime.code`), backed by the
//!   `codira_ai` engine. A deliberately small
//!   slice: `f32` tensors created from a caller's buffer and shape, matmul,
//!   broadcasting add, relu, softmax, reading a tensor back, freeing it; and
//!   one training path, a `Linear` layer (`y = x W + b`) trained by one
//!   step of plain SGD on the mean squared error, through the engine's
//!   reverse-mode autodiff. Every function is registered in each runtime's
//!   dispatch table.
//!
//! The conventions are those of `native_fs` and `native_db`:
//! addresses and lengths are `usize`, every failure is a return value, never
//! a panic, and each body runs under `catch_unwind`, so a bug here becomes
//! [`AI_ERR_OTHER`] instead of unwinding into generated code. Functions that
//! produce a handle or a number return an `i64` (the value when it is zero or
//! more, a negative `AI_ERR_*` code otherwise); the others return an `i32`
//! (`0` or a code).
//!
//! Tensor and layer handles are generation-checked entries of two tables
//! with distinct kind tags (`native_handles`). Tensors are immutable values:
//! every operation returns a *new* handle and leaves its operands as they
//! were. Nothing is freed automatically: the garbage collector does not
//! know these handles, so a program frees every tensor it creates with
//! [`codira_ai_tensor_free`] (and every layer with [`codira_ai_linear_free`]);
//! a handle not freed lives until the process ends.
//!
//! Memory passed by address is raw memory, as for the `load_*`/`store_*`
//! intrinsics: the runtime checks that addresses are not null and lengths are
//! representable, and trusts the caller for the rest. Element data is `f32`
//! in row-major order; shapes are arrays of `usize` (`u64` on 64-bit hosts).
//! Neither needs to be aligned.
//!
//! No tensor may hold more than [`AI_MAX_ELEMENTS`] elements; operations
//! whose result would be larger are refused with [`AI_ERR_TOO_LARGE`] before
//! anything is allocated. Below that limit an allocation failure aborts the
//! process, as it does everywhere in Rust's standard library.
//!
//! Floating point follows `codira_ai`'s policy (its README): summation in
//! increasing index order, no reassociation, no FMA contraction; the blocked
//! matmul is bit-identical to the naive loop on the tested machine.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

use codira_ai::{
    nn::{init::Init, layers::Linear, loss::mse, ParamStore},
    optim::{Optimizer, Sgd},
    tensor::broadcast_shapes,
    Graph, Rng, Tensor, TensorError,
};
use parking_lot::Mutex;

use crate::{native_handles::HandleTable, FunctionDefinition, IntoFunctionDefinition};

/// The highest rank a tensor may have.
pub const AI_MAX_RANK: usize = 8;
/// The most elements one tensor may hold (2^28, 1 GiB of `f32`).
pub const AI_MAX_ELEMENTS: usize = 1 << 28;

/// An unexpected internal failure (a caught panic).
pub const AI_ERR_OTHER: i32 = -1;
/// The handle is not a live tensor (or layer) of the expected kind: never
/// created, freed, or forged.
pub const AI_ERR_BAD_HANDLE: i32 = -2;
/// A null address with a non-zero length, a rank above [`AI_MAX_RANK`], a
/// zero layer dimension, or a learning rate that is not finite and positive.
pub const AI_ERR_INVALID_ARGUMENT: i32 = -3;
/// Shapes that must agree do not (for example a layer input whose last axis
/// is not the layer's input width, or a matmul operand of rank below 2).
pub const AI_ERR_SHAPE: i32 = -4;
/// Two shapes that cannot be broadcast together (`NumPy`'s rules).
pub const AI_ERR_BROADCAST: i32 = -5;
/// A matmul whose inner dimensions differ.
pub const AI_ERR_MATMUL_INNER: i32 = -6;
/// An axis (or index) out of range.
pub const AI_ERR_OUT_OF_RANGE: i32 = -7;
/// The caller's buffer holds fewer elements than the tensor; nothing was
/// written. [`codira_ai_tensor_numel`] gives the count needed.
pub const AI_ERR_BUFFER_TOO_SMALL: i32 = -8;
/// The runtime's table of tensors or layers is full.
pub const AI_ERR_TOO_MANY_HANDLES: i32 = -9;
/// The result would hold more than [`AI_MAX_ELEMENTS`] elements.
pub const AI_ERR_TOO_LARGE: i32 = -10;

/// A `Linear` layer and the store that holds its parameters.
struct LinearModel {
    store: ParamStore<f32>,
    layer: Linear,
}

static TENSORS: Mutex<HandleTable<Tensor<f32>>> = Mutex::new(HandleTable::new(3));
static LINEARS: Mutex<HandleTable<Arc<Mutex<LinearModel>>>> = Mutex::new(HandleTable::new(4));

/// Maps an engine error to an `AI_ERR_*` code.
fn error_code(error: &TensorError) -> i32 {
    match error {
        TensorError::DataLength { .. }
        | TensorError::ShapeMismatch { .. }
        | TensorError::ReshapeCount { .. }
        | TensorError::NotAView { .. }
        | TensorError::RankTooLow { .. }
        | TensorError::NonScalarLoss { .. } => AI_ERR_SHAPE,
        TensorError::Broadcast { .. } => AI_ERR_BROADCAST,
        TensorError::MatmulInner { .. } => AI_ERR_MATMUL_INNER,
        TensorError::AxisOutOfRange { .. }
        | TensorError::InvalidPermutation { .. }
        | TensorError::SliceOutOfRange { .. }
        | TensorError::IndexOutOfRange { .. } => AI_ERR_OUT_OF_RANGE,
        TensorError::InvalidArgument { .. } => AI_ERR_INVALID_ARGUMENT,
        // Custom gradients are not reachable through these services.
        TensorError::BadCustomGradient { .. } => AI_ERR_OTHER,
    }
}

/// Runs `body`, turning a panic into [`AI_ERR_OTHER`].
fn guarded<R: From<i32>>(body: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| R::from(AI_ERR_OTHER))
}

/// The number of elements of `shape`, if it is at most [`AI_MAX_ELEMENTS`].
fn checked_numel(shape: &[usize]) -> Result<usize, i32> {
    shape
        .iter()
        .try_fold(1usize, |count, &dim| count.checked_mul(dim))
        .filter(|&count| count <= AI_MAX_ELEMENTS)
        .ok_or(AI_ERR_TOO_LARGE)
}

fn tensor_for(handle: i64) -> Result<Tensor<f32>, i32> {
    TENSORS.lock().get(handle).cloned().ok_or(AI_ERR_BAD_HANDLE)
}

fn linear_for(handle: i64) -> Result<Arc<Mutex<LinearModel>>, i32> {
    LINEARS.lock().get(handle).cloned().ok_or(AI_ERR_BAD_HANDLE)
}

/// Stores a result tensor and returns its handle.
fn publish(result: Result<Tensor<f32>, i32>) -> i64 {
    match result {
        Ok(tensor) => TENSORS
            .lock()
            .insert(tensor)
            .unwrap_or_else(|_| i64::from(AI_ERR_TOO_MANY_HANDLES)),
        Err(code) => i64::from(code),
    }
}

/// Reads `count` unaligned values of `T` at `addr`.
///
/// # Safety
///
/// `addr..addr + count * size_of::<T>()` must be readable, and `addr` must
/// not be null unless `count` is 0; any bit pattern must be a valid `T`.
unsafe fn read_array<T: Copy + Default>(addr: usize, count: usize) -> Vec<T> {
    let mut out = vec![T::default(); count];
    if count != 0 {
        // Safety: the caller's contract; `out` is our own allocation of
        // `count` values.
        unsafe {
            std::ptr::copy_nonoverlapping(
                addr as *const u8,
                out.as_mut_ptr().cast::<u8>(),
                count * std::mem::size_of::<T>(),
            );
        }
    }
    out
}

/// Creates a tensor of shape `shape_addr[0..rank]` (`usize` values) holding
/// a copy of the `f32` values at `data_addr`, row-major (as many as the
/// product of the shape; a rank-0 tensor holds one). Returns a tensor handle
/// (positive) or a negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_tensor_new(data_addr: usize, shape_addr: usize, rank: usize) -> i64 {
    guarded(|| {
        if rank > AI_MAX_RANK || (shape_addr == 0 && rank != 0) {
            return i64::from(AI_ERR_INVALID_ARGUMENT);
        }
        // Safety: the program passes `rank` readable `usize` values.
        let shape: Vec<usize> = unsafe { read_array(shape_addr, rank) };
        let count = match checked_numel(&shape) {
            Ok(count) => count,
            Err(code) => return i64::from(code),
        };
        if data_addr == 0 && count != 0 {
            return i64::from(AI_ERR_INVALID_ARGUMENT);
        }
        // Safety: the program passes `count` readable `f32` values;
        // `count <= AI_MAX_ELEMENTS`, so the byte length fits.
        let data: Vec<f32> = unsafe { read_array(data_addr, count) };
        publish(Tensor::from_vec(data, &shape).map_err(|error| error_code(&error)))
    })
}

/// The rank of a tensor, or [`AI_ERR_BAD_HANDLE`].
pub extern "C" fn codira_ai_tensor_rank(tensor: i64) -> i64 {
    guarded(|| match tensor_for(tensor) {
        Ok(tensor) => tensor.rank() as i64,
        Err(code) => i64::from(code),
    })
}

/// The extent of axis `axis` of a tensor, or a negative `AI_ERR_*` code
/// ([`AI_ERR_OUT_OF_RANGE`] if `axis` is not below the rank).
pub extern "C" fn codira_ai_tensor_dim(tensor: i64, axis: usize) -> i64 {
    guarded(|| match tensor_for(tensor) {
        Ok(tensor) => match tensor.shape().get(axis) {
            Some(&dim) => dim as i64,
            None => i64::from(AI_ERR_OUT_OF_RANGE),
        },
        Err(code) => i64::from(code),
    })
}

/// The number of elements of a tensor, or [`AI_ERR_BAD_HANDLE`].
pub extern "C" fn codira_ai_tensor_numel(tensor: i64) -> i64 {
    guarded(|| match tensor_for(tensor) {
        Ok(tensor) => tensor.numel() as i64,
        Err(code) => i64::from(code),
    })
}

/// Copies a tensor's elements, row-major, as `f32` to `out_addr`, which has
/// room for `capacity` elements. Returns the number of elements written, or
/// a negative `AI_ERR_*` code ([`AI_ERR_BUFFER_TOO_SMALL`], writing nothing,
/// if `capacity` is below the element count).
pub extern "C" fn codira_ai_tensor_read(tensor: i64, out_addr: usize, capacity: usize) -> i64 {
    guarded(|| {
        let tensor = match tensor_for(tensor) {
            Ok(tensor) => tensor,
            Err(code) => return i64::from(code),
        };
        let count = tensor.numel();
        if count > capacity {
            return i64::from(AI_ERR_BUFFER_TOO_SMALL);
        }
        if count == 0 {
            return 0;
        }
        if out_addr == 0 {
            return i64::from(AI_ERR_INVALID_ARGUMENT);
        }
        let values = tensor.to_vec();
        // Safety: the program passes room for `capacity >= count` `f32`s;
        // `values` is our own copy, so the ranges do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                values.as_ptr().cast::<u8>(),
                out_addr as *mut u8,
                count * std::mem::size_of::<f32>(),
            );
        }
        count as i64
    })
}

/// Frees a tensor handle. Returns `0` or [`AI_ERR_BAD_HANDLE`]; the handle
/// is invalid afterwards. Tensors made from it are not affected.
pub extern "C" fn codira_ai_tensor_free(tensor: i64) -> i32 {
    guarded(|| match TENSORS.lock().remove(tensor) {
        Some(_) => 0,
        None => AI_ERR_BAD_HANDLE,
    })
}

/// `a @ b`: matrix product over the last two axes, with broadcast batch
/// axes (both operands need rank 2 or more). Returns a new tensor handle or
/// a negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_matmul(a: i64, b: i64) -> i64 {
    guarded(|| {
        let (a, b) = match (tensor_for(a), tensor_for(b)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(code), _) | (_, Err(code)) => return i64::from(code),
        };
        let (sa, sb) = (a.shape(), b.shape());
        if sa.len() >= 2 && sb.len() >= 2 && sa[sa.len() - 1] == sb[sb.len() - 2] {
            // Refuse an oversized result before the engine allocates it.
            if let Ok(mut out) =
                broadcast_shapes("matmul", &sa[..sa.len() - 2], &sb[..sb.len() - 2])
            {
                out.extend([sa[sa.len() - 2], sb[sb.len() - 1]]);
                if let Err(code) = checked_numel(&out) {
                    return i64::from(code);
                }
            }
        }
        publish(a.matmul(&b).map_err(|error| error_code(&error)))
    })
}

/// `a + b` element-wise, broadcasting under `NumPy`'s rules. Returns a new
/// tensor handle or a negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_add(a: i64, b: i64) -> i64 {
    guarded(|| {
        let (a, b) = match (tensor_for(a), tensor_for(b)) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(code), _) | (_, Err(code)) => return i64::from(code),
        };
        if let Ok(out) = broadcast_shapes("add", a.shape(), b.shape()) {
            if let Err(code) = checked_numel(&out) {
                return i64::from(code);
            }
        }
        publish(a.add(&b).map_err(|error| error_code(&error)))
    })
}

/// `max(x, 0)` element-wise. Returns a new tensor handle or
/// [`AI_ERR_BAD_HANDLE`].
pub extern "C" fn codira_ai_relu(tensor: i64) -> i64 {
    guarded(|| publish(tensor_for(tensor).map(|tensor| tensor.relu())))
}

/// The softmax along axis `axis` (computed as `exp(x - max)` over its sum,
/// so it does not overflow for finite inputs). Returns a new tensor handle or
/// a negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_softmax(tensor: i64, axis: usize) -> i64 {
    guarded(|| {
        publish(
            tensor_for(tensor)
                .and_then(|tensor| tensor.softmax(axis).map_err(|error| error_code(&error))),
        )
    })
}

/// Creates a `Linear` layer `y = x W + b` with `W: [in_dim, out_dim]` drawn
/// from the Xavier uniform distribution by the engine's seeded generator
/// (xoshiro256**, not cryptographic) and `b: [out_dim]` zero. The same seed
/// gives the same weights. Returns a layer handle or a negative `AI_ERR_*`
/// code.
pub extern "C" fn codira_ai_linear_new(in_dim: usize, out_dim: usize, seed: u64) -> i64 {
    guarded(|| {
        if in_dim == 0 || out_dim == 0 {
            return i64::from(AI_ERR_INVALID_ARGUMENT);
        }
        if let Err(code) = checked_numel(&[in_dim, out_dim]) {
            return i64::from(code);
        }
        let mut store = ParamStore::new();
        let mut rng = Rng::new(seed);
        let layer = Linear::new(
            &mut store,
            "linear",
            in_dim,
            out_dim,
            Init::Xavier,
            &mut rng,
        );
        LINEARS
            .lock()
            .insert(Arc::new(Mutex::new(LinearModel { store, layer })))
            .unwrap_or_else(|_| i64::from(AI_ERR_TOO_MANY_HANDLES))
    })
}

/// Applies a layer to `x: [..., in_dim]` (rank 2 or more, e.g. a batch
/// `[n, in_dim]`), returning a new tensor handle of shape `[..., out_dim]` or
/// a negative `AI_ERR_*` code ([`AI_ERR_SHAPE`] if the last axis of `x` is
/// not `in_dim` or its rank is below 2).
pub extern "C" fn codira_ai_linear_forward(linear: i64, x: i64) -> i64 {
    guarded(|| {
        let (model, x) = match (linear_for(linear), tensor_for(x)) {
            (Ok(model), Ok(x)) => (model, x),
            (Err(code), _) | (_, Err(code)) => return i64::from(code),
        };
        let model = model.lock();
        if x.shape().last() != Some(&model.layer.in_dim) {
            return i64::from(AI_ERR_SHAPE);
        }
        let graph = Graph::new();
        let params = model.store.bind(&graph);
        publish(
            model
                .layer
                .forward(&params, graph.constant(x))
                .map(|y| y.value())
                .map_err(|error| error_code(&error)),
        )
    })
}

/// A copy of a layer's weight `W: [in_dim, out_dim]` as a new tensor handle,
/// or a negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_linear_weight(linear: i64) -> i64 {
    guarded(|| {
        publish(linear_for(linear).map(|model| {
            let model = model.lock();
            model.store.get(model.layer.w).clone()
        }))
    })
}

/// A copy of a layer's bias `b: [out_dim]` as a new tensor handle, or a
/// negative `AI_ERR_*` code.
pub extern "C" fn codira_ai_linear_bias(linear: i64) -> i64 {
    guarded(|| {
        publish(linear_for(linear).and_then(|model| {
            let model = model.lock();
            match model.layer.b {
                Some(b) => Ok(model.store.get(b).clone()),
                None => Err(AI_ERR_OTHER),
            }
        }))
    })
}

/// One training step on a batch `x: [..., in_dim]` (rank 2 or more):
/// computes `loss = mean((x W + b - y)^2)` (`y` broadcasts against the
/// prediction `[..., out_dim]`), its gradient by reverse-mode
/// autodiff, and updates `W` and `b` by plain SGD (`p -= lr * grad`, no
/// momentum or weight decay). Writes the loss *before* the update as an
/// `f32` to `loss_out_addr` unless it is null. Returns `0` or a negative
/// `AI_ERR_*` code; on an error the layer is unchanged.
pub extern "C" fn codira_ai_linear_mse_sgd_step(
    linear: i64,
    x: i64,
    y: i64,
    lr: f32,
    loss_out_addr: usize,
) -> i32 {
    guarded(|| {
        if !(lr.is_finite() && lr > 0.0) {
            return AI_ERR_INVALID_ARGUMENT;
        }
        let (model, x, y) = match (linear_for(linear), tensor_for(x), tensor_for(y)) {
            (Ok(model), Ok(x), Ok(y)) => (model, x, y),
            (Err(code), _, _) | (_, Err(code), _) | (_, _, Err(code)) => return code,
        };
        let mut model = model.lock();
        if x.shape().last() != Some(&model.layer.in_dim) {
            return AI_ERR_SHAPE;
        }
        let step = || -> Result<(f32, Vec<Tensor<f32>>), TensorError> {
            let graph = Graph::new();
            let params = model.store.bind(&graph);
            let prediction = model.layer.forward(&params, graph.constant(x))?;
            let loss = mse(prediction, graph.constant(y))?;
            let value = loss.value().item()?;
            let grads = params.grads(&graph.backward(loss)?);
            Ok((value, grads))
        };
        let (loss, grads) = match step() {
            Ok(result) => result,
            Err(error) => return error_code(&error),
        };
        let mut sgd = Sgd::new(f64::from(lr), 0.0, 0.0);
        if let Err(error) = sgd.step(&mut model.store, &grads) {
            return error_code(&error);
        }
        if loss_out_addr != 0 {
            // Safety: the program passes room for one `f32` (module docs).
            unsafe { std::ptr::write_unaligned(loss_out_addr as *mut f32, loss) };
        }
        0
    })
}

/// Frees a layer handle. Returns `0` or [`AI_ERR_BAD_HANDLE`].
pub extern "C" fn codira_ai_linear_free(linear: i64) -> i32 {
    guarded(|| match LINEARS.lock().remove(linear) {
        Some(_) => 0,
        None => AI_ERR_BAD_HANDLE,
    })
}

/// Returns the definitions of every function in this module, for the
/// runtime's dispatch table.
pub(crate) fn functions() -> Vec<FunctionDefinition> {
    vec![
        IntoFunctionDefinition::into(
            codira_ai_tensor_new as extern "C" fn(usize, usize, usize) -> i64,
            "codira_ai_tensor_new",
        ),
        IntoFunctionDefinition::into(
            codira_ai_tensor_rank as extern "C" fn(i64) -> i64,
            "codira_ai_tensor_rank",
        ),
        IntoFunctionDefinition::into(
            codira_ai_tensor_dim as extern "C" fn(i64, usize) -> i64,
            "codira_ai_tensor_dim",
        ),
        IntoFunctionDefinition::into(
            codira_ai_tensor_numel as extern "C" fn(i64) -> i64,
            "codira_ai_tensor_numel",
        ),
        IntoFunctionDefinition::into(
            codira_ai_tensor_read as extern "C" fn(i64, usize, usize) -> i64,
            "codira_ai_tensor_read",
        ),
        IntoFunctionDefinition::into(
            codira_ai_tensor_free as extern "C" fn(i64) -> i32,
            "codira_ai_tensor_free",
        ),
        IntoFunctionDefinition::into(
            codira_ai_matmul as extern "C" fn(i64, i64) -> i64,
            "codira_ai_matmul",
        ),
        IntoFunctionDefinition::into(
            codira_ai_add as extern "C" fn(i64, i64) -> i64,
            "codira_ai_add",
        ),
        IntoFunctionDefinition::into(
            codira_ai_relu as extern "C" fn(i64) -> i64,
            "codira_ai_relu",
        ),
        IntoFunctionDefinition::into(
            codira_ai_softmax as extern "C" fn(i64, usize) -> i64,
            "codira_ai_softmax",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_new as extern "C" fn(usize, usize, u64) -> i64,
            "codira_ai_linear_new",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_forward as extern "C" fn(i64, i64) -> i64,
            "codira_ai_linear_forward",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_weight as extern "C" fn(i64) -> i64,
            "codira_ai_linear_weight",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_bias as extern "C" fn(i64) -> i64,
            "codira_ai_linear_bias",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_mse_sgd_step as extern "C" fn(i64, i64, i64, f32, usize) -> i32,
            "codira_ai_linear_mse_sgd_step",
        ),
        IntoFunctionDefinition::into(
            codira_ai_linear_free as extern "C" fn(i64) -> i32,
            "codira_ai_linear_free",
        ),
    ]
}

#[cfg(test)]
mod tests;
