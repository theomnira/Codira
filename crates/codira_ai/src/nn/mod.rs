//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Parameters ([`ParamStore`], [`ParamId`], [`Bound`]): where trainable
//!   tensors live between steps, and how they enter a fresh [`Graph`].
//! - [`init`]: Xavier/Glorot and Kaiming/He initialisers with a seeded RNG.
//! - [`layers`]: `Linear`, `Embedding`, `LayerNorm`, `Mlp`.
//! - [`attention`]: scaled dot-product attention, multi-head attention with
//!   an optional causal mask, and a pre-norm transformer block.
//! - [`loss`]: cross-entropy and mean squared error.
//!
//! # The training-step protocol
//!
//! Layers hold [`ParamId`]s, not tensors. One step is:
//!
//! ```
//! # use codira_ai::{nn::{ParamStore, layers::Linear, init::Init}, Graph, Rng, Tensor};
//! let mut rng = Rng::new(0);
//! let mut store = ParamStore::<f64>::new();
//! let lin = Linear::new(&mut store, "fc", 3, 2, Init::Xavier, &mut rng);
//! let g = Graph::new();                       // a fresh tape per step
//! let p = store.bind(&g);                     // parameters become leaves
//! let x = g.constant(Tensor::ones(&[4, 3]));
//! let loss = lin.forward(&p, x).unwrap().tanh().mean();
//! let grads = p.grads(&g.backward(loss).unwrap()); // one per parameter
//! assert_eq!(grads.len(), store.len());
//! ```
//!
//! and an optimiser then updates `store` from `grads`.

pub mod attention;
pub mod init;
pub mod layers;
pub mod loss;

use crate::{
    autograd::{Gradients, Graph, Var},
    float::Float,
    tensor::Tensor,
};

/// Index of a parameter in a [`ParamStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParamId(pub usize);

/// The trainable tensors of a model, in registration order.
#[derive(Clone, Debug, Default)]
pub struct ParamStore<T: Float> {
    values: Vec<Tensor<T>>,
    names: Vec<String>,
}

impl<T: Float> ParamStore<T> {
    /// No parameters.
    pub fn new() -> Self {
        ParamStore {
            values: Vec::new(),
            names: Vec::new(),
        }
    }

    /// Registers a parameter.
    pub fn add(&mut self, name: impl Into<String>, value: Tensor<T>) -> ParamId {
        self.values.push(value);
        self.names.push(name.into());
        ParamId(self.values.len() - 1)
    }

    /// The number of parameters (tensors, not scalars).
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// `true` if there are no parameters.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The total number of scalars.
    pub fn numel(&self) -> usize {
        self.values.iter().map(Tensor::numel).sum()
    }

    /// A parameter's current value.
    pub fn get(&self, id: ParamId) -> &Tensor<T> {
        &self.values[id.0]
    }

    /// A parameter's name.
    pub fn name(&self, id: ParamId) -> &str {
        &self.names[id.0]
    }

    /// All values, in registration order.
    pub fn values(&self) -> &[Tensor<T>] {
        &self.values
    }

    /// All values, mutably (for optimisers).
    pub fn values_mut(&mut self) -> &mut [Tensor<T>] {
        &mut self.values
    }

    /// Records every parameter as a [`Graph::param`] leaf of `g`.
    pub fn bind<'g>(&self, g: &'g Graph<T>) -> Bound<'g, T> {
        Bound {
            vars: self.values.iter().map(|v| g.param(v.clone())).collect(),
        }
    }
}

/// The parameters of a [`ParamStore`] as variables of one graph.
pub struct Bound<'g, T: Float> {
    vars: Vec<Var<'g, T>>,
}

impl<'g, T: Float> Bound<'g, T> {
    /// Uses caller-made variables as the parameters, in registration
    /// order (for gradient checking a whole model).
    pub fn from_vars(vars: Vec<Var<'g, T>>) -> Self {
        Bound { vars }
    }

    /// The variable for a parameter.
    pub fn var(&self, id: ParamId) -> Var<'g, T> {
        self.vars[id.0]
    }

    /// The gradient for every parameter, in registration order (zeros for
    /// parameters the loss does not depend on).
    pub fn grads(&self, grads: &Gradients<T>) -> Vec<Tensor<T>> {
        self.vars.iter().map(|&v| grads.wrt(v)).collect()
    }
}
