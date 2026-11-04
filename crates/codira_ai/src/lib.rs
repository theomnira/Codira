//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - `codira_ai`: the native tensor and automatic-differentiation engine
//!   that the Codira runtime is to expose to `std.ai`. Pure Rust, `std`
//!   only: no BLAS, no external array library, no Python, no inference
//!   service.
//!
//! # Modules
//!
//! | module | contents |
//! |--------|----------|
//! | [`tensor`] | strided N-d tensors, views, broadcasting, reductions, blocked matmul |
//! | [`autograd`] | tape-based reverse mode, custom gradients, gradient checking, dual numbers |
//!
//! # Status
//!
//! Experimental (Tier B candidate, see `lab/rfcs/0003-codira-ai.md`). No
//! Codira program can reach this crate yet: the runtime C ABI binding is
//! future work owned by the runtime workstream.

pub mod autograd;
pub mod error;
pub mod float;
pub mod rng;
pub mod tensor;

pub use autograd::{Graph, Var};
pub use error::{Result, TensorError};
pub use float::Float;
pub use rng::Rng;
pub use tensor::Tensor;
