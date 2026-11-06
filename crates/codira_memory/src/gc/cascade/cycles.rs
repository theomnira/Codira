//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Trial deletion (Bacon and Rajan, "Concurrent Cycle Collection in
//!   Reference Counted Systems", ECOOP 2001): MarkGray, Scan and
//!   CollectWhite over a candidate set, on trial counts kept in a side map
//!   so the real counts are never modified, with a work budget.
//! - Validation of a cycle set computed while mutators ran (the Σ- and
//!   Δ-tests of the concurrent variant).
//!
//! The same code serves the owner thread's synchronous pass over its
//! unpublished objects and the collector's concurrent pass over published
//! ones; the difference is the [`CycleGraph`] view: which objects are in
//! scope and what counts as an external reference.

use rustc_hash::{FxHashMap, FxHashSet};

/// A view of the object graph for one pass. Objects are header addresses.
pub(crate) trait CycleGraph {
    /// The count of `object` for this pass (heap references plus whatever
    /// the view treats as external, such as local holds), or `None` if the
    /// object is outside the pass's scope (it is then neither traversed nor
    /// a member, and edges to it are ignored).
    fn count(&self, object: usize) -> Option<u32>;

    /// Calls `f` with every reference stored in `object`.
    fn children(&self, object: usize, f: &mut dyn FnMut(usize));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Gray,
    Black,
    White,
}

/// Why a pass gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PassError {
    /// The pass needed more work than its budget allowed.
    OverBudget,
}

/// Runs trial deletion from `roots` and returns the objects that are
/// garbage *according to the view's counts* (the white set). Every visited
/// object and every traversed edge costs one unit of `budget`; when the
/// budget runs out the pass is abandoned. `work` receives the units used.
///
/// For a view with exact counts and no concurrent mutation the white set is
/// garbage (Bacon and Rajan, Theorem 1). A concurrent caller must validate
/// it ([`validate`]) before reclaiming anything.
pub(crate) fn trial_deletion(
    graph: &dyn CycleGraph,
    roots: &[usize],
    budget: usize,
    work: &mut usize,
) -> Result<Vec<usize>, PassError> {
    let mut nodes: FxHashMap<usize, (i64, Color)> = FxHashMap::default();
    let mut used = 0usize;
    let charge = |units: usize, used: &mut usize| -> Result<(), PassError> {
        *used += units;
        if *used > budget {
            Err(PassError::OverBudget)
        } else {
            Ok(())
        }
    };
    let mut children = Vec::new();
    let result = (|| {
        // MarkGray: every reachable in-scope object turns gray once, and
        // every edge between them subtracts one from its target.
        let mut stack = Vec::new();
        for &root in roots {
            if nodes.contains_key(&root) {
                continue;
            }
            let Some(count) = graph.count(root) else {
                continue;
            };
            charge(1, &mut used)?;
            nodes.insert(root, (i64::from(count), Color::Gray));
            stack.push(root);
            while let Some(object) = stack.pop() {
                children.clear();
                graph.children(object, &mut |child| children.push(child));
                for &child in &children {
                    charge(1, &mut used)?;
                    if let Some(entry) = nodes.get_mut(&child) {
                        entry.0 -= 1;
                        continue;
                    }
                    let Some(count) = graph.count(child) else {
                        continue;
                    };
                    nodes.insert(child, (i64::from(count) - 1, Color::Gray));
                    stack.push(child);
                }
            }
        }

        // Scan: a gray object with a positive trial count has an external
        // reference; it and everything it reaches are live (ScanBlack
        // restores the counts it subtracted). The others turn white.
        let mut scan: Vec<usize> = roots.to_vec();
        while let Some(object) = scan.pop() {
            let Some(&(count, color)) = nodes.get(&object) else {
                continue;
            };
            if color != Color::Gray {
                continue;
            }
            if count > 0 {
                scan_black(graph, &mut nodes, object, &mut children, &mut used, budget)?;
            } else {
                nodes.get_mut(&object).expect("visited").1 = Color::White;
                children.clear();
                graph.children(object, &mut |child| children.push(child));
                charge(children.len(), &mut used)?;
                scan.extend(children.iter().copied());
            }
        }
        Ok(())
    })();
    *work += used.min(budget);
    result?;

    // CollectWhite
    let mut white: Vec<usize> = nodes
        .iter()
        .filter(|(_, &(_, color))| color == Color::White)
        .map(|(&object, _)| object)
        .collect();
    white.sort_unstable();
    Ok(white)
}

fn scan_black(
    graph: &dyn CycleGraph,
    nodes: &mut FxHashMap<usize, (i64, Color)>,
    object: usize,
    children: &mut Vec<usize>,
    used: &mut usize,
    budget: usize,
) -> Result<(), PassError> {
    let mut stack = vec![object];
    nodes.get_mut(&object).expect("visited").1 = Color::Black;
    while let Some(object) = stack.pop() {
        children.clear();
        graph.children(object, &mut |child| children.push(child));
        *used += children.len();
        if *used > budget {
            return Err(PassError::OverBudget);
        }
        for &child in children.iter() {
            if let Some(entry) = nodes.get_mut(&child) {
                entry.0 += 1;
                if entry.1 != Color::Black {
                    entry.1 = Color::Black;
                    stack.push(child);
                }
            }
        }
    }
    Ok(())
}

/// The Σ-test: every member's count equals the number of references to it
/// from members (read now), so the set has no external heap reference.
/// `count` returns `None` for a member that must not be reclaimed for a
/// reason only the caller knows (the Δ-test: touched since the set was
/// computed, zero, rooted). Returns the units of work used.
pub(crate) fn validate(
    members: &[usize],
    count: &dyn Fn(usize) -> Option<u32>,
    children: &dyn Fn(usize, &mut dyn FnMut(usize)),
) -> (bool, usize) {
    let set: FxHashSet<usize> = members.iter().copied().collect();
    let mut internal: FxHashMap<usize, u32> = FxHashMap::default();
    let mut work = members.len();
    for &member in members {
        children(member, &mut |child| {
            work += 1;
            if set.contains(&child) {
                *internal.entry(child).or_default() += 1;
            }
        });
    }
    for &member in members {
        let Some(count) = count(member) else {
            return (false, work);
        };
        if count != internal.get(&member).copied().unwrap_or(0) {
            return (false, work);
        }
    }
    (true, work)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test graph: `edges[i]` are the children of object `i`, `external[i]`
    /// counts references from outside the graph.
    struct Graph {
        edges: Vec<Vec<usize>>,
        external: Vec<u32>,
    }

    impl Graph {
        fn in_degree(&self, object: usize) -> u32 {
            self.edges
                .iter()
                .map(|children| children.iter().filter(|&&c| c == object).count() as u32)
                .sum()
        }

        /// The objects reachable from an externally referenced one.
        fn live(&self) -> FxHashSet<usize> {
            let mut live = FxHashSet::default();
            let mut stack: Vec<usize> = (0..self.edges.len())
                .filter(|&i| self.external[i] > 0)
                .collect();
            while let Some(object) = stack.pop() {
                if live.insert(object) {
                    stack.extend(self.edges[object].iter().copied());
                }
            }
            live
        }
    }

    impl CycleGraph for Graph {
        fn count(&self, object: usize) -> Option<u32> {
            Some(self.in_degree(object) + self.external[object])
        }

        fn children(&self, object: usize, f: &mut dyn FnMut(usize)) {
            for &child in &self.edges[object] {
                f(child);
            }
        }
    }

    #[test]
    fn simple_cycle_is_white() {
        let graph = Graph {
            edges: vec![vec![1], vec![2], vec![0]],
            external: vec![0, 0, 0],
        };
        let mut work = 0;
        assert_eq!(
            trial_deletion(&graph, &[0], 100, &mut work),
            Ok(vec![0, 1, 2])
        );
    }

    #[test]
    fn externally_referenced_cycle_is_black() {
        let graph = Graph {
            edges: vec![vec![1], vec![2], vec![0, 3], vec![]],
            external: vec![0, 1, 0, 0],
        };
        let mut work = 0;
        assert_eq!(trial_deletion(&graph, &[0], 100, &mut work), Ok(vec![]));
    }

    #[test]
    fn budget_is_respected() {
        let n = 100;
        let graph = Graph {
            edges: (0..n).map(|i| vec![(i + 1) % n]).collect(),
            external: vec![0; n],
        };
        let mut work = 0;
        assert_eq!(
            trial_deletion(&graph, &[0], 50, &mut work),
            Err(PassError::OverBudget)
        );
        assert!(work <= 50);
    }

    /// Exhaustive: every directed graph on 4 nodes (2^16 edge sets) with
    /// every subset of externally referenced nodes. With exact counts, the
    /// white set from all roots is exactly the unreachable nodes that lie on
    /// or below a cycle reachable from the roots.
    #[test]
    fn exhaustive_small_graphs() {
        const N: usize = 4;
        for edge_bits in 0u32..(1 << (N * N)) {
            let edges: Vec<Vec<usize>> = (0..N)
                .map(|from| {
                    (0..N)
                        .filter(|&to| edge_bits & (1 << (from * N + to)) != 0)
                        .collect()
                })
                .collect();
            for external_bits in 0u32..(1 << N) {
                let graph = Graph {
                    edges: edges.clone(),
                    external: (0..N).map(|i| (external_bits >> i) & 1).collect(),
                };
                let live = graph.live();
                let roots: Vec<usize> = (0..N).collect();
                let mut work = 0;
                let white = trial_deletion(&graph, &roots, usize::MAX, &mut work).unwrap();
                // Never white when live.
                for &object in &white {
                    assert!(!live.contains(&object), "{edge_bits:#x} {external_bits:#x}");
                }
                // Complete: every dead node is white (all nodes are roots).
                for object in 0..N {
                    if !live.contains(&object) {
                        assert!(white.contains(&object), "{edge_bits:#x} {external_bits:#x}");
                    }
                }
                // The validator accepts exactly the dead sets.
                if !white.is_empty() {
                    let (ok, _) = validate(
                        &white,
                        &|o| graph.count(o),
                        &|o, f| graph.children(o, f),
                    );
                    assert!(ok);
                }
                if !live.is_empty() {
                    let live: Vec<usize> = live.iter().copied().collect();
                    let (ok, _) =
                        validate(&live, &|o| graph.count(o), &|o, f| graph.children(o, f));
                    assert!(!ok, "a live set must fail the Σ-test");
                }
            }
        }
    }
}
