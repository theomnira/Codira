//! Recovery Graphs (RG): semantically-safe control-flow DAGs encoding all
//! possible recovery paths from a guarded point.
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality (per Section 3.4 of the HERACLES spec):
//! - `RecoveryNode`: executable strategies including the nominal path, with an
//!   optional guard predicate and a cost model.
//! - `RecoveryEdge`: either a `FaultEdge` (fault condition between nodes) or a
//!   `PostconditionEdge` (a recovered result failing the postcondition).
//! - `RecoveryGraph`: a DAG over nodes and edges, with edge weight update
//!   support so the Bayesian ranker can reorder recovery paths at runtime
//!   (Section 5.3: "Recovery Graph Edge Weight Update").
//!
//! The graph guarantees acyclicity by construction: edges only ever point
//! from a lower index to a higher index, so any walk is terminating. This is
//! the executable analogue of the paper's "all DAG paths are proven acyclic
//! and terminating via structural induction" (Section 4.2.3).

#![allow(clippy::match_same_arms, clippy::type_complexity, clippy::unused_self)]
//! Lint notes: several matches here are *tables* mapping distinct
//! strategies/faults onto a shared tier or recovery action -- collapsing
//! the arms would erase which case is which. The `type_complexity`
//! sites are graph adjacency maps whose shape is the point.

use std::collections::HashMap;

/// A node in a recovery graph: one executable strategy.
pub struct RecoveryNode {
    /// A name for this node (e.g. "nominal", "retry", "fallback").
    pub name: String,
    /// A guard predicate over the recovered result: whether this node's
    /// output is acceptable to the caller. If `None`, the node's output is
    /// always acceptable.
    pub guard: Option<Box<dyn Fn(&[u8]) -> bool + Send + Sync>>,
    /// A rational cost model for this strategy (higher = more expensive).
    /// The ranker uses this alongside success probability.
    pub cost: u64,
    /// Whether this is the nominal (fault-free) path.
    pub is_nominal: bool,
}

impl RecoveryNode {
    pub fn new(name: impl Into<String>) -> Self {
        RecoveryNode {
            name: name.into(),
            guard: None,
            cost: 0,
            is_nominal: false,
        }
    }
}

/// A directed edge in a recovery graph, connecting two [`RecoveryNode`]s.
pub enum RecoveryEdge {
    /// A fault condition: from `source`, if `fault` occurs (and the edge's
    /// state guard holds), transition to `target`.
    FaultEdge {
        source: usize,
        target: usize,
        fault: u32,
        guard: Option<Box<dyn Fn(&[u8]) -> bool + Send + Sync>>,
        weight: f64,
    },
    /// A postcondition failure: from `source`, if the recovered result
    /// fails `condition`, transition to `target`.
    PostconditionEdge {
        source: usize,
        target: usize,
        condition: Box<dyn Fn(&[u8]) -> bool + Send + Sync>,
        weight: f64,
    },
}

impl RecoveryEdge {
    /// The source node index.
    pub fn source(&self) -> usize {
        match self {
            RecoveryEdge::FaultEdge { source, .. } => *source,
            RecoveryEdge::PostconditionEdge { source, .. } => *source,
        }
    }

    /// The target node index.
    pub fn target(&self) -> usize {
        match self {
            RecoveryEdge::FaultEdge { target, .. } => *target,
            RecoveryEdge::PostconditionEdge { target, .. } => *target,
        }
    }

    /// The current weight of this edge, used by the ranker.
    pub fn weight(&self) -> f64 {
        match self {
            RecoveryEdge::FaultEdge { weight, .. } => *weight,
            RecoveryEdge::PostconditionEdge { weight, .. } => *weight,
        }
    }

    /// Updates the weight of this edge in place.
    pub fn set_weight(&mut self, weight: f64) {
        match self {
            RecoveryEdge::FaultEdge { weight: w, .. } => *w = weight,
            RecoveryEdge::PostconditionEdge { weight: w, .. } => *w = weight,
        }
    }
}

/// A semantically-safe recovery graph: a DAG of [`RecoveryNode`]s connected
/// by [`RecoveryEdge`]s.
#[derive(Default)]
pub struct RecoveryGraph {
    nodes: Vec<RecoveryNode>,
    edges: Vec<RecoveryEdge>,
    adjacency: HashMap<usize, Vec<usize>>,
}

impl RecoveryGraph {
    /// Creates an empty recovery graph.
    pub fn new() -> Self {
        RecoveryGraph::default()
    }

    /// Adds a node and returns its index.
    pub fn add_node(&mut self, node: RecoveryNode) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        index
    }

    /// Adds an edge. Returns `false` (and does nothing) if the edge would
    /// create a cycle: edges are only accepted when they point strictly
    /// forward in index order, which guarantees the graph stays a DAG.
    pub fn add_edge(&mut self, edge: RecoveryEdge) -> bool {
        let (source, target) = (edge.source(), edge.target());
        if source >= self.nodes.len() || target >= self.nodes.len() {
            return false;
        }
        if target <= source {
            // Backward / self edge would break acyclicity.
            return false;
        }
        let weight = edge.weight();
        self.edges.push(edge);
        self.adjacency
            .entry(source)
            .or_default()
            .push(self.edges.len() - 1);
        let _ = weight;
        true
    }

    /// All nodes in the graph.
    pub fn nodes(&self) -> &[RecoveryNode] {
        &self.nodes
    }

    /// All edges in the graph.
    pub fn edges(&self) -> &[RecoveryEdge] {
        &self.edges
    }

    /// The out-edges (as edge indices) of the given node.
    pub fn outgoing_edges(&self, node: usize) -> Vec<usize> {
        self.adjacency.get(&node).cloned().unwrap_or_default()
    }

    /// The successors reachable from `node` in one hop.
    pub fn successors(&self, node: usize) -> Vec<usize> {
        self.outgoing_edges(node)
            .into_iter()
            .filter_map(|e| self.edges.get(e))
            .map(RecoveryEdge::target)
            .collect()
    }

    /// Updates the weight of the edge at index `edge_index`, as the
    /// Bayesian ranker does when reordering recovery paths (Section 5.3).
    pub fn set_edge_weight(&mut self, edge_index: usize, weight: f64) -> bool {
        if let Some(edge) = self.edges.get_mut(edge_index) {
            edge.set_weight(weight);
            true
        } else {
            false
        }
    }

    /// The number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The number of edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_accepts_forward_edges() {
        let mut g = RecoveryGraph::new();
        let nominal = g.add_node(RecoveryNode::new("nominal"));
        let fallback = g.add_node(RecoveryNode::new("fallback"));
        let edge = RecoveryEdge::FaultEdge {
            source: nominal,
            target: fallback,
            fault: 0,
            guard: None,
            weight: 1.0,
        };
        assert!(g.add_edge(edge));
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn graph_rejects_cycles() {
        let mut g = RecoveryGraph::new();
        let a = g.add_node(RecoveryNode::new("a"));
        let b = g.add_node(RecoveryNode::new("b"));
        // forward edge a -> b is fine
        assert!(g.add_edge(RecoveryEdge::PostconditionEdge {
            source: a,
            target: b,
            condition: Box::new(|_| true),
            weight: 1.0,
        }));
        // backward edge b -> a would create a cycle: rejected
        assert!(!g.add_edge(RecoveryEdge::PostconditionEdge {
            source: b,
            target: a,
            condition: Box::new(|_| true),
            weight: 1.0,
        }));
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn edges_can_be_reweighted() {
        let mut g = RecoveryGraph::new();
        let a = g.add_node(RecoveryNode::new("a"));
        let b = g.add_node(RecoveryNode::new("b"));
        g.add_edge(RecoveryEdge::FaultEdge {
            source: a,
            target: b,
            fault: 0,
            guard: None,
            weight: 1.0,
        });
        assert!(g.set_edge_weight(0, 0.5));
        assert_eq!(g.edges()[0].weight(), 0.5);
    }

    #[test]
    fn successors_follow_outgoing_edges() {
        let mut g = RecoveryGraph::new();
        let a = g.add_node(RecoveryNode::new("a"));
        let b = g.add_node(RecoveryNode::new("b"));
        let c = g.add_node(RecoveryNode::new("c"));
        g.add_edge(RecoveryEdge::FaultEdge {
            source: a,
            target: b,
            fault: 0,
            guard: None,
            weight: 1.0,
        });
        g.add_edge(RecoveryEdge::FaultEdge {
            source: a,
            target: c,
            fault: 1,
            guard: None,
            weight: 1.0,
        });
        let mut succs = g.successors(a);
        succs.sort_unstable();
        assert_eq!(succs, vec![1, 2]);
    }
}
