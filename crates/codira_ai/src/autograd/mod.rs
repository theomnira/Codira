//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 4, 2026
//!
//! Functionality:
//! - Reverse-mode automatic differentiation on a tape ([`Graph`]), the
//!   variable handle [`Var`], gradient accumulation ([`Gradients`]) and the
//!   custom-gradient hook [`Graph::custom`].
//! - [`check`]: numerical gradient checking by central differences, the
//!   oracle every rule in [`ops`] is tested against.
//! - [`dual`]: forward-mode derivatives with dual numbers.
//!
//! # The tape
//!
//! A [`Graph`] is an append-only list of nodes. Each node holds its value,
//! the ids of its parents, and (if any parent needs a gradient) a boxed
//! closure mapping the gradient of the node to the gradients of its
//! parents. Because nodes can only refer to earlier nodes, the list is
//! already in topological order and [`Graph::backward`] is a single
//! reverse sweep, with no graph search and no recursion.
//!
//! A gradient a rule returns always has exactly its parent's shape: rules
//! over broadcasting ops sum over the broadcast axes
//! ([`Tensor::reduce_to_shape`]). The sweep checks this for every rule,
//! including custom ones, and reports a [`TensorError::BadCustomGradient`]
//! instead of accumulating a wrongly shaped gradient.
//!
//! One graph records one computation. To train, build a fresh graph per
//! step (see `nn::ParamStore::bind`); a graph is not `Send`.

pub mod check;
pub mod dual;
pub mod ops;

use std::cell::RefCell;

use crate::{
    error::{Result, TensorError},
    float::Float,
    tensor::Tensor,
};

/// A gradient rule: gradient of the node in, gradients of the parents out.
pub type BackwardFn<T> = Box<dyn Fn(&Tensor<T>) -> Result<Vec<Tensor<T>>>>;

struct Node<T: Float> {
    value: Tensor<T>,
    parents: Vec<usize>,
    backward: Option<BackwardFn<T>>,
    requires_grad: bool,
}

/// A tape of recorded operations. See the module documentation.
pub struct Graph<T: Float> {
    nodes: RefCell<Vec<Node<T>>>,
}

/// A handle to a node of a [`Graph`]. Cheap to copy.
pub struct Var<'g, T: Float> {
    graph: &'g Graph<T>,
    id: usize,
}

impl<T: Float> Clone for Var<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Float> Copy for Var<'_, T> {}

impl<T: Float> std::fmt::Debug for Var<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Var(#{}, shape={:?})", self.id, self.shape())
    }
}

impl<T: Float> Default for Graph<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Float> Graph<T> {
    /// An empty tape.
    pub fn new() -> Self {
        Graph {
            nodes: RefCell::new(Vec::new()),
        }
    }

    /// The number of recorded nodes.
    pub fn len(&self) -> usize {
        self.nodes.borrow().len()
    }

    /// `true` if nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push_node(&self, value: Tensor<T>, parents: Vec<usize>, backward: Option<BackwardFn<T>>, requires_grad: bool) -> Var<'_, T> {
        let mut nodes = self.nodes.borrow_mut();
        let id = nodes.len();
        nodes.push(Node {
            value,
            parents,
            backward: if requires_grad { backward } else { None },
            requires_grad,
        });
        Var { graph: self, id }
    }

    /// A leaf whose gradient [`Graph::backward`] computes (a parameter or
    /// an input being differentiated).
    pub fn param(&self, value: Tensor<T>) -> Var<'_, T> {
        self.push_node(value, vec![], None, true)
    }

    /// A leaf treated as a constant: no gradient flows into it.
    pub fn constant(&self, value: Tensor<T>) -> Var<'_, T> {
        self.push_node(value, vec![], None, false)
    }

    /// Records a node computed from `parents`, with gradient rule `rule`.
    /// The rule is dropped (never called) when no parent needs a gradient.
    pub(crate) fn record(&self, value: Tensor<T>, parents: &[Var<'_, T>], rule: BackwardFn<T>) -> Var<'_, T> {
        let requires = {
            let nodes = self.nodes.borrow();
            parents.iter().any(|p| nodes[p.id].requires_grad)
        };
        self.push_node(value, parents.iter().map(|p| p.id).collect(), Some(rule), requires)
    }

    /// The custom-gradient hook: records `value` as a function of `inputs`
    /// whose vector-Jacobian product is `rule(grad_out, input_values)`.
    /// `rule` must return one gradient per input, each with that input's
    /// shape; [`Graph::backward`] checks both and fails with
    /// [`TensorError::BadCustomGradient`] otherwise.
    pub fn custom<'g>(
        &'g self,
        inputs: &[Var<'g, T>],
        value: Tensor<T>,
        rule: impl Fn(&Tensor<T>, &[Tensor<T>]) -> Result<Vec<Tensor<T>>> + 'static,
    ) -> Var<'g, T> {
        let values: Vec<Tensor<T>> = inputs.iter().map(|v| v.value()).collect();
        self.record(value, inputs, Box::new(move |g| rule(g, &values)))
    }

    /// Reverse sweep from a single-element `loss`. Returns the gradient of
    /// the loss with respect to every leaf created with [`Graph::param`]
    /// that the loss depends on.
    pub fn backward(&self, loss: Var<'_, T>) -> Result<Gradients<T>> {
        let nodes = self.nodes.borrow();
        let root = &nodes[loss.id];
        if root.value.numel() != 1 {
            return Err(TensorError::NonScalarLoss {
                shape: root.value.shape().to_vec(),
            });
        }
        let mut grads: Vec<Option<Tensor<T>>> = vec![None; loss.id + 1];
        grads[loss.id] = Some(Tensor::ones(root.value.shape()));
        for id in (0..=loss.id).rev() {
            let node = &nodes[id];
            let Some(rule) = &node.backward else { continue };
            let Some(g) = grads[id].take() else { continue };
            let parent_grads = rule(&g)?;
            if parent_grads.len() != node.parents.len() {
                return Err(TensorError::BadCustomGradient {
                    detail: format!(
                        "node #{id} has {} inputs but its rule returned {} gradients",
                        node.parents.len(),
                        parent_grads.len()
                    ),
                });
            }
            for (&p, pg) in node.parents.iter().zip(parent_grads) {
                let parent = &nodes[p];
                if !parent.requires_grad {
                    continue;
                }
                if pg.shape() != parent.value.shape() {
                    return Err(TensorError::BadCustomGradient {
                        detail: format!(
                            "node #{id} returned a gradient of shape {:?} for input #{p} of shape {:?}",
                            pg.shape(),
                            parent.value.shape()
                        ),
                    });
                }
                grads[p] = Some(match grads[p].take() {
                    Some(acc) => acc.add(&pg)?,
                    None => pg,
                });
            }
        }
        Ok(Gradients { grads })
    }
}

/// The result of [`Graph::backward`]: gradients of the leaves.
pub struct Gradients<T: Float> {
    grads: Vec<Option<Tensor<T>>>,
}

impl<T: Float> Gradients<T> {
    /// The gradient with respect to `v`, if the loss depends on it.
    pub fn get(&self, v: Var<'_, T>) -> Option<&Tensor<T>> {
        self.grads.get(v.id).and_then(|g| g.as_ref())
    }

    /// The gradient with respect to `v`, or zeros if the loss does not
    /// depend on it.
    pub fn wrt(&self, v: Var<'_, T>) -> Tensor<T> {
        self.get(v).cloned().unwrap_or_else(|| Tensor::zeros(v.value().shape()))
    }
}

impl<'g, T: Float> Var<'g, T> {
    /// The graph this variable belongs to.
    pub fn graph(&self) -> &'g Graph<T> {
        self.graph
    }

    /// The node's value (a cheap, shared copy).
    pub fn value(&self) -> Tensor<T> {
        self.graph.nodes.borrow()[self.id].value.clone()
    }

    /// The node's shape.
    pub fn shape(&self) -> Vec<usize> {
        self.graph.nodes.borrow()[self.id].value.shape().to_vec()
    }

    /// `true` if gradients flow through this node.
    pub fn requires_grad(&self) -> bool {
        self.graph.nodes.borrow()[self.id].requires_grad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_scalar_loss_is_rejected() {
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::ones(&[2]));
        assert!(matches!(g.backward(x), Err(TensorError::NonScalarLoss { .. })));
    }

    #[test]
    fn gradients_accumulate_over_reuse() {
        // f(x) = sum(x * x + x) => df/dx = 2x + 1
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::from_vec(vec![1.0, -2.0, 3.0], &[3]).unwrap());
        let y = x.mul(x).unwrap().add(x).unwrap().sum();
        let grads = g.backward(y).unwrap();
        assert_eq!(grads.wrt(x).to_vec(), vec![3.0, -3.0, 7.0]);
    }

    #[test]
    fn constants_get_no_gradient_and_no_rule() {
        let g = Graph::<f64>::new();
        let c = g.constant(Tensor::ones(&[2]));
        let y = c.exp().sum();
        assert!(!y.requires_grad());
        let grads = g.backward(y).unwrap();
        assert!(grads.get(c).is_none());
        assert_eq!(grads.wrt(c).to_vec(), vec![0.0, 0.0]);
    }

    #[test]
    fn custom_gradient_hook_is_used_and_validated() {
        // A straight-through estimator: forward rounds, backward is identity.
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::from_vec(vec![0.4, 1.6], &[2]).unwrap());
        let rounded = x.value().map(|v| v.round());
        let y = g.custom(&[x], rounded, |grad, _| Ok(vec![grad.clone()]));
        assert_eq!(y.value().to_vec(), vec![0.0, 2.0]);
        let loss = y.scale(3.0).sum();
        assert_eq!(g.backward(loss).unwrap().wrt(x).to_vec(), vec![3.0, 3.0]);

        // a rule returning the wrong shape is reported, not accumulated
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::ones(&[2]));
        let y = g.custom(&[x], x.value(), |_, _| Ok(vec![Tensor::ones(&[3])]));
        assert!(matches!(g.backward(y.sum()), Err(TensorError::BadCustomGradient { .. })));
        let g = Graph::<f64>::new();
        let x = g.param(Tensor::ones(&[2]));
        let y = g.custom(&[x], x.value(), |_, _| Ok(vec![]));
        assert!(matches!(g.backward(y.sum()), Err(TensorError::BadCustomGradient { .. })));
    }
}
