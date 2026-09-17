//! Copyright (c) 2026 Omnira CJSC. All Rights Reserved.
//! Author: Tunjay Akbarli
//! Date: September 15, 2026
//!
//! Extraction: choosing the cheapest program out of the saturated
//! e-graph, then rebuilding it as a [`Body`].
//!
//! # Why a fixpoint, not a single bottom-up pass
//!
//! After saturation an e-graph is typically **cyclic**: `x` and `x + 0`
//! and `(x + 0) + 0` all live in one class, so following a class's nodes
//! can lead back to the class itself. A single bottom-up traversal would
//! not terminate, and a naive depth-first walk could pick a node whose
//! cost depends on its own class.
//!
//! So costs are computed by **relaxation**, the way a shortest-path
//! algorithm handles cycles: every class starts at infinite cost, and each
//! round recomputes `cost(class) = min over its nodes of (node cost + sum
//! of children costs)`, using the previous round's values. Costs only ever
//! decrease and are bounded below, so the iteration converges; when a
//! round improves nothing, the assignment is optimal for this cost model.
//! A class that is still infinite afterwards is unreachable-by-any-finite-
//! term (only possible through a malformed graph) and falls back to its
//! first node.
//!
//! # The cost model
//!
//! Costs approximate issue latency on a typical pipelined integer unit:
//! constants and references are free-to-cheap, add/sub/bitwise/shift cost
//! one, multiply costs three, divide and remainder cost eight. Opaque ops
//! (control flow, calls) cost one plus their operands -- their region
//! bodies were optimized separately, and their *identity* is fixed, so no
//! choice is being made about them here.
//!
//! This is the same "search the space, pick by cost model" structure
//! KGEN's design document describes for kernel-generator expansion
//! (`modular/KGEN/docs/DesignOverview.md`); the cost model is the knob
//! that decides what "optimized" means, and it is deliberately explicit
//! and in one place rather than implicit in the order of a pass pipeline.

use codira_mir::{Body, OpId, OpKind};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::egraph::{is_pure, EClassId, EGraph, ENode, NodeKind};

/// Cost of an operator itself, excluding its operands.
#[allow(clippy::match_same_arms)]
fn node_cost(graph: &EGraph, node: &ENode) -> u64 {
    match &node.kind {
        NodeKind::Pure(kind) => match kind {
            // A literal is free: it folds into an instruction's immediate
            // field, and preferring it is what makes constant folding
            // actually win during extraction.
            OpKind::Const(_) => 0,
            OpKind::Arg(_) | OpKind::BlockArg(_) | OpKind::ParamRef(_) => 1,
            OpKind::Mul => 3,
            OpKind::Div | OpKind::Rem => 8,
            _ => 1,
        },
        NodeKind::Opaque(serial) => {
            // Opaque ops are not chosen against alternatives, but their
            // cost still has to dominate their operands' so extraction
            // does not prefer duplicating work into them.
            let _ = serial;
            let _ = graph;
            1
        }
    }
}

const INFINITY: u64 = u64::MAX / 4;

/// Per-class best choice: the node to extract and its total cost.
struct Choice {
    node: ENode,
    cost: u64,
}

/// Extracts the cheapest term rooted at `root` as a fresh [`Body`].
pub(crate) fn extract(graph: &EGraph, root: EClassId) -> Body {
    let best = compute_costs(graph);

    let mut body = Body::new();
    let mut built: FxHashMap<EClassId, OpId> = FxHashMap::default();
    let result = build(graph, &best, graph.find_const(root), &mut body, &mut built);

    // A term whose value is an already-built shared subexpression may not
    // be the last op in the arena, but `Body::result()` is positional. Re-
    // materializing the value keeps the body's result correct without a
    // dedicated copy op -- the same identity `codira_mir::pass::rewrite`
    // uses for this, and for the same reason.
    if let Some(result) = result {
        if body.result() != Some(result) {
            // The wrapper is transparent, so it carries the wrapped
            // value's type (the Tuple itself is a one-element aggregate
            // and stays untyped -- nothing consumes it but the projection).
            let ty = body.get(result).ty;
            let tuple = body.push(OpKind::Tuple, [result]);
            body.push_typed(OpKind::TupleGet(0), [tuple], ty);
        }
    }
    body
}

/// Cost relaxation to fixpoint -- see the module doc.
fn compute_costs(graph: &EGraph) -> FxHashMap<EClassId, Choice> {
    let classes = graph.class_ids();
    let mut best: FxHashMap<EClassId, Choice> = FxHashMap::default();

    loop {
        let mut improved = false;
        for &class in &classes {
            let canon = graph.find_const(class);
            for node in graph.nodes(canon) {
                // Sum children using the *previous* round's costs; a child
                // with no cost yet makes this node unusable this round.
                let mut total = node_cost(graph, node);
                let mut usable = true;
                for &child in &node.children {
                    match best.get(&graph.find_const(child)) {
                        Some(c) if c.cost < INFINITY => total = total.saturating_add(c.cost),
                        _ => {
                            usable = false;
                            break;
                        }
                    }
                }
                if !usable {
                    continue;
                }
                let better = match best.get(&canon) {
                    None => true,
                    Some(current) => total < current.cost,
                };
                if better {
                    best.insert(
                        canon,
                        Choice {
                            node: node.clone(),
                            cost: total,
                        },
                    );
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }

    // Any class still without a choice is only reachable through a cycle
    // with no finite base case -- a malformed graph. Fall back to its
    // first node so extraction is total rather than panicking.
    for &class in &classes {
        let canon = graph.find_const(class);
        if let std::collections::hash_map::Entry::Vacant(e) = best.entry(canon) {
            if let Some(node) = graph.nodes(canon).first() {
                e.insert(Choice {
                    node: node.clone(),
                    cost: INFINITY,
                });
            }
        }
    }
    best
}

/// Emits the chosen term for `class` into `body`, memoizing so a shared
/// subexpression is emitted exactly once (extraction produces a DAG, not
/// a tree).
fn build(
    graph: &EGraph,
    best: &FxHashMap<EClassId, Choice>,
    class: EClassId,
    body: &mut Body,
    built: &mut FxHashMap<EClassId, OpId>,
) -> Option<OpId> {
    let canon = graph.find_const(class);
    if let Some(&id) = built.get(&canon) {
        return Some(id);
    }
    let choice = best.get(&canon)?;

    let mut operands: SmallVec<[OpId; 2]> = SmallVec::new();
    for &child in &choice.node.children {
        operands.push(build(graph, best, child, body, built)?);
    }

    let id = match &choice.node.kind {
        NodeKind::Pure(kind) => {
            debug_assert!(is_pure(kind), "non-pure op reached pure extraction");
            body.push_typed(kind.clone(), operands, choice.node.ty)
        }
        NodeKind::Opaque(serial) => {
            let original = graph.opaque.get(*serial as usize)?;
            body.push_with_regions_typed(
                original.kind.clone(),
                operands,
                original.regions.iter().cloned(),
                original.ty,
            )
        }
    };
    built.insert(canon, id);
    Some(id)
}
