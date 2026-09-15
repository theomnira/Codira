//! Copyright (c) 2026 Omnira CJSC. All Rights Reserved.
//! Author: Tunjay Akbarli
//! Date: September 15, 2026
//!
//! The e-graph itself: union-find, hashconsing, congruence closure, and
//! the e-class analyses.
//!
//! Follows egg (Willsey et al., POPL 2021) closely, in particular its
//! central performance idea: congruence is **not** maintained eagerly on
//! every union. Unions are recorded, the affected classes are pushed onto
//! a worklist, and [`EGraph::rebuild`] restores the congruence invariant
//! in a batch afterwards. Eager maintenance forces a hash-lookup storm per
//! union; deferred rebuilding lets one pass fix them all, and is the
//! difference between an e-graph that saturates in milliseconds and one
//! that does not finish.

use codira_mir::{fold_op, Attr, Body, Op, OpId, OpKind, Region, TypeId};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use crate::{EGraphOptimizer, SaturationReport, TypeEnv};

/// An e-class: an equivalence class of e-nodes, all proven to compute the
/// same value. Ids are *not* stable under [`EGraph::union`] -- always
/// canonicalize with [`EGraph::find`] before comparing or storing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EClassId(pub u32);

/// One e-node: an operation applied to e-*classes* (not to other nodes).
/// That indirection is what lets a single node stand for exponentially
/// many concrete expressions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ENode {
    pub kind: NodeKind,
    pub children: SmallVec<[EClassId; 2]>,
    /// The type of the value this node computes.
    ///
    /// Part of the hashcons key, deliberately: two nodes with identical
    /// operators and children but *different types* must never be
    /// congruent (RFC-001 section 1.7 edge case 5). Including the type
    /// here makes cross-type merging structurally impossible rather than
    /// something an analysis has to police.
    ///
    /// `TypeId::UNTYPED` during the phase-1/2 migration.
    pub ty: TypeId,
}

/// An e-node's operator.
///
/// Pure ops carry their [`OpKind`] directly, so two structurally identical
/// pure computations hashcons to the same node. Opaque ops (control flow,
/// calls) instead carry a **unique serial**: two distinct `cf.if`s must
/// never be considered equal just because they happen to have the same
/// operands, since their region bodies differ. The serial indexes
/// [`EGraph::opaque`], which holds the original op.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum NodeKind {
    Pure(OpKind),
    Opaque(u32),
}

/// Per-class facts computed bottom-up, merged on union. This is egg's
/// "e-class analysis": a monotone lattice whose value is maintained
/// automatically as the graph grows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Analysis {
    /// The class's value, when it is a compile-time constant. Constant
    /// folding *is* an analysis here rather than a rewrite rule: when a
    /// class is discovered to be constant, the corresponding `core.const`
    /// node is added to it, making the constant available to extraction
    /// and to every rule that needs a literal.
    pub constant: Option<Attr>,
    /// `true` when this class provably holds an integer. Gates every
    /// arithmetic-restructuring rule -- see the crate doc for why this is
    /// a soundness requirement, not an optimization.
    pub definitely_int: bool,
}

impl Analysis {
    /// Merges the analyses of two classes being unioned. Both facts are
    /// "more information wins": a class known constant on either side is
    /// constant; likewise definitely-int.
    fn merge(&mut self, other: &Analysis) -> bool {
        let mut changed = false;
        if self.constant.is_none() {
            if let Some(c) = &other.constant {
                self.constant = Some(c.clone());
                changed = true;
            }
        } else if let (Some(a), Some(b)) = (&self.constant, &other.constant) {
            // Two different constants in one class means a rule is
            // unsound. Assert in debug; in release keep the first, which
            // at worst forgoes an optimization.
            debug_assert_eq!(a, b, "unsound rewrite merged two distinct constants");
        }
        if !self.definitely_int && other.definitely_int {
            self.definitely_int = true;
            changed = true;
        }
        changed
    }
}

pub struct EGraph {
    /// Union-find parent pointers, indexed by raw class id.
    parents: Vec<EClassId>,
    /// Union-by-rank ranks, parallel to `parents`.
    ranks: Vec<u8>,
    /// Canonical class id -> its member nodes.
    classes: FxHashMap<EClassId, Vec<ENode>>,
    /// Canonical class id -> its analysis.
    analyses: FxHashMap<EClassId, Analysis>,
    /// Hashcons: canonical node -> the class containing it.
    memo: FxHashMap<ENode, EClassId>,
    /// For each class, the nodes that *reference* it as a child, paired
    /// with the class those nodes belong to.
    ///
    /// This is what makes congruence work. When two classes merge, it is
    /// not their own nodes that become non-canonical -- it is the nodes
    /// *above* them: given `f(a)` and `f(b)`, unioning `a` with `b` is
    /// what makes those two `f` nodes congruent, and they are only
    /// reachable from `a`/`b` through this parent list.
    parents_of: FxHashMap<EClassId, Vec<(ENode, EClassId)>>,
    /// Classes merged since the last rebuild, whose parents therefore
    /// need repairing (egg's deferred-congruence worklist).
    pending: Vec<EClassId>,
    /// Original ops behind [`NodeKind::Opaque`] serials, with their region
    /// bodies already recursively optimized.
    pub(crate) opaque: Vec<Op>,
    /// What the caller knows about argument types (see [`TypeEnv`]).
    types: TypeEnv,
}

impl Default for EGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl EGraph {
    pub fn new() -> Self {
        Self::with_types(TypeEnv::unknown())
    }

    /// An e-graph whose analyses may use the caller's type knowledge.
    pub fn with_types(types: TypeEnv) -> Self {
        EGraph {
            types,
            parents: Vec::new(),
            ranks: Vec::new(),
            classes: FxHashMap::default(),
            analyses: FxHashMap::default(),
            memo: FxHashMap::default(),
            parents_of: FxHashMap::default(),
            pending: Vec::new(),
            opaque: Vec::new(),
        }
    }

    pub fn num_nodes(&self) -> usize {
        self.classes.values().map(Vec::len).sum()
    }

    pub fn num_classes(&self) -> usize {
        self.classes.len()
    }

    // ---- union-find --------------------------------------------------------

    /// The canonical representative of `id`, with path compression.
    pub fn find(&mut self, id: EClassId) -> EClassId {
        let mut root = id;
        while self.parents[root.0 as usize] != root {
            root = self.parents[root.0 as usize];
        }
        // Path compression: re-point every node on the path at the root.
        let mut current = id;
        while self.parents[current.0 as usize] != root {
            let next = self.parents[current.0 as usize];
            self.parents[current.0 as usize] = root;
            current = next;
        }
        root
    }

    /// Non-mutating lookup, for read-only passes (extraction). Requires a
    /// prior [`EGraph::rebuild`] to be meaningful.
    pub(crate) fn find_const(&self, id: EClassId) -> EClassId {
        let mut root = id;
        while self.parents[root.0 as usize] != root {
            root = self.parents[root.0 as usize];
        }
        root
    }

    fn new_class(&mut self, node: ENode, analysis: Analysis) -> EClassId {
        let id = EClassId(self.parents.len() as u32);
        self.parents.push(id);
        self.ranks.push(0);
        self.classes.insert(id, vec![node]);
        self.analyses.insert(id, analysis);
        id
    }

    /// Merges the classes of `a` and `b`. Returns `true` when they were
    /// not already equal. Congruence is restored later by
    /// [`EGraph::rebuild`], not here.
    pub fn union(&mut self, a: EClassId, b: EClassId) -> bool {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        // Union by rank keeps the find-path shallow.
        if self.ranks[ra.0 as usize] < self.ranks[rb.0 as usize] {
            std::mem::swap(&mut ra, &mut rb);
        }
        if self.ranks[ra.0 as usize] == self.ranks[rb.0 as usize] {
            self.ranks[ra.0 as usize] += 1;
        }
        self.parents[rb.0 as usize] = ra;

        let nodes = self.classes.remove(&rb).unwrap_or_default();
        self.classes.entry(ra).or_default().extend(nodes);

        // The merged class inherits both parent lists: every node that
        // referenced either class now references the survivor.
        let parents = self.parents_of.remove(&rb).unwrap_or_default();
        self.parents_of.entry(ra).or_default().extend(parents);

        if let Some(other) = self.analyses.remove(&rb) {
            let mut mine = self.analyses.remove(&ra).unwrap_or(Analysis {
                constant: None,
                definitely_int: false,
            });
            mine.merge(&other);
            self.analyses.insert(ra, mine);
        }

        self.pending.push(ra);
        true
    }

    fn canonicalize(&mut self, node: &ENode) -> ENode {
        ENode {
            kind: node.kind.clone(),
            children: node.children.iter().map(|&c| self.find(c)).collect(),
            ty: node.ty,
        }
    }

    /// Restores the congruence invariant: if two nodes have equal
    /// operators and (canonically) equal children, their classes are
    /// equal. This is egg's deferred rebuild, run in a loop until the
    /// worklist drains, since merging two classes can make *their* parents
    /// congruent in turn.
    pub fn rebuild(&mut self) {
        while !self.pending.is_empty() {
            let todo: Vec<EClassId> = std::mem::take(&mut self.pending);
            let mut seen = FxHashSet::default();
            for id in todo {
                let id = self.find(id);
                if seen.insert(id) {
                    self.repair(id);
                }
            }
        }
    }

    /// Restores congruence around one just-merged class, by
    /// re-canonicalizing every node that references it: if two such
    /// parent nodes become identical after canonicalization, their
    /// classes are congruent and must be merged (which enqueues *their*
    /// parents in turn, hence the loop in [`EGraph::rebuild`]).
    fn repair(&mut self, id: EClassId) {
        // Re-canonicalize this class's own nodes and dedupe them, so the
        // class does not keep stale children around.
        let nodes = self.classes.remove(&id).unwrap_or_default();
        let mut fresh: Vec<ENode> = Vec::with_capacity(nodes.len());
        for node in nodes {
            let canon = self.canonicalize(&node);
            if !fresh.contains(&canon) {
                fresh.push(canon);
            }
        }
        self.classes.insert(id, fresh);

        // Re-memoize the parents under their new canonical form, merging
        // any pair that collides.
        let parents = self.parents_of.remove(&id).unwrap_or_default();
        let mut deduped: FxHashMap<ENode, EClassId> = FxHashMap::default();
        for (node, owner) in parents {
            // The old key is stale now that a child moved.
            self.memo.remove(&node);
            let canon = self.canonicalize(&node);
            let owner = self.find(owner);

            if let Some(&existing) = deduped.get(&canon) {
                // Two parents of this class just became identical --
                // congruence says their classes are equal.
                self.union(existing, owner);
            }
            let owner = self.find(owner);
            deduped.insert(canon.clone(), owner);
            self.memo.insert(canon, owner);
        }
        self.parents_of.insert(id, deduped.into_iter().collect());
    }

    // ---- construction ------------------------------------------------------

    /// Adds a node, returning its class (existing, if hashconsed).
    pub(crate) fn add(&mut self, node: ENode) -> EClassId {
        let canon = self.canonicalize(&node);
        if let Some(&existing) = self.memo.get(&canon) {
            return self.find(existing);
        }
        let analysis = self.analyze(&canon);
        let id = self.new_class(canon.clone(), analysis.clone());
        self.memo.insert(canon.clone(), id);

        // Register this node with each of its children, so a later merge
        // of a child can find and re-canonicalize it (see `parents_of`).
        for &child in &canon.children {
            let child = self.find(child);
            self.parents_of
                .entry(child)
                .or_default()
                .push((canon.clone(), id));
        }

        // A class discovered to be constant gets the literal added to it,
        // so extraction can pick the cheap form and rules can match on it.
        if let Some(value) = analysis.constant {
            let const_node = ENode {
                kind: NodeKind::Pure(OpKind::Const(value)),
                children: SmallVec::new(),
                ty: canon.ty,
            };
            if self.memo.get(&const_node).copied() != Some(id) {
                let const_id = if let Some(existing) = self.memo.get(&const_node).copied() {
                    self.find(existing)
                } else {
                    let a = self.analyze(&const_node);
                    let cid = self.new_class(const_node.clone(), a);
                    self.memo.insert(const_node, cid);
                    cid
                };
                self.union(id, const_id);
            }
        }
        self.find(id)
    }

    /// Computes a fresh node's analysis from its children's.
    #[allow(clippy::match_same_arms)]
    fn analyze(&self, node: &ENode) -> Analysis {
        let NodeKind::Pure(kind) = &node.kind else {
            // Opaque ops: an unknown value of unknown type.
            return Analysis {
                constant: None,
                definitely_int: false,
            };
        };

        let child_constants: Option<Vec<Attr>> = node
            .children
            .iter()
            .map(|&c| {
                self.analyses
                    .get(&self.find_const(c))
                    .and_then(|a| a.constant.clone())
            })
            .collect();

        let constant = match kind {
            OpKind::Const(attr) => Some(attr.clone()),
            _ => match child_constants {
                // A genuine evaluation error (division by zero) must NOT
                // become a constant -- the program is supposed to fail
                // there, and folding it away would erase the failure.
                // `NotFoldable` is simply "not a pure computation". Both
                // outcomes are therefore "no constant".
                Some(attrs) => fold_op(kind, &attrs).ok(),
                None => None,
            },
        };

        let children_int = || {
            node.children.iter().all(|&c| {
                self.analyses
                    .get(&self.find_const(c))
                    .is_some_and(|a| a.definitely_int)
            })
        };

        let definitely_int = match kind {
            // Distinct reasons for the same answer; see each comment.
            OpKind::Const(Attr::Int(_)) => true,
            // Type knowledge the frontend handed down (see `TypeEnv`).
            OpKind::Arg(i) => self.types.is_int_arg(*i),
            OpKind::BlockArg(i) => self.types.is_int_block_arg(*i),
            // `fold_op` rejects float operands for these outright, so a
            // well-formed program only ever applies them to integers.
            OpKind::BitAnd | OpKind::BitOr | OpKind::BitXor | OpKind::Shl | OpKind::Shr => true,
            // Int-preserving arithmetic: integer iff every operand is.
            OpKind::Add | OpKind::Sub | OpKind::Mul | OpKind::Div | OpKind::Rem | OpKind::Neg => {
                children_int()
            }
            _ => false,
        };

        Analysis {
            constant,
            definitely_int,
        }
    }

    pub(crate) fn analysis(&self, id: EClassId) -> Option<&Analysis> {
        self.analyses.get(&self.find_const(id))
    }

    /// The type of a class, taken from any of its nodes (all nodes in
    /// a class share a type -- see [`ENode::ty`]).
    pub(crate) fn class_ty(&self, id: EClassId) -> TypeId {
        self.nodes(id).first().map_or(TypeId::UNTYPED, |n| n.ty)
    }

    pub(crate) fn nodes(&self, id: EClassId) -> &[ENode] {
        self.classes
            .get(&self.find_const(id))
            .map_or(&[][..], Vec::as_slice)
    }

    pub(crate) fn class_ids(&self) -> Vec<EClassId> {
        let mut ids: Vec<EClassId> = self.classes.keys().copied().collect();
        // Deterministic order: saturation must not depend on hash order.
        ids.sort_unstable();
        ids
    }

    /// Adds every op of `body`, returning the class of its result op.
    /// Bodies nested inside opaque ops are optimized recursively first.
    pub(crate) fn add_body(&mut self, body: &Body, opt: &EGraphOptimizer) -> Option<EClassId> {
        let mut ids: FxHashMap<OpId, EClassId> = FxHashMap::default();
        for (id, op) in body.iter() {
            let class = self.add_op(op, &ids, opt)?;
            ids.insert(id, class);
        }
        body.result().and_then(|r| ids.get(&r).copied())
    }

    fn add_op(
        &mut self,
        op: &Op,
        ids: &FxHashMap<OpId, EClassId>,
        opt: &EGraphOptimizer,
    ) -> Option<EClassId> {
        let children: Option<SmallVec<[EClassId; 2]>> =
            op.operands.iter().map(|o| ids.get(o).copied()).collect();
        let children = children?;

        if is_pure(&op.kind) {
            return Some(self.add(ENode {
                kind: NodeKind::Pure(op.kind.clone()),
                children,
                ty: op.ty,
            }));
        }

        // `cf.yield` is a loop-region terminator, not a value: it has no
        // meaningful e-class. Bodies containing one at top level are
        // rejected (the caller returns them untouched).
        if matches!(op.kind, OpKind::Yield) {
            return None;
        }

        // Opaque: recursively optimize the regions, then store the op.
        let regions: SmallVec<[Region; 0]> = op
            .regions
            .iter()
            .map(|r| {
                // Nested regions inherit the caller's type knowledge:
                // `core.arg` means the same thing at every depth.
                let (optimized, _) = opt.optimize_body_with(&r.body, &self.types);
                Region::with_args(r.num_args, optimized)
            })
            .collect();
        let serial = self.opaque.len() as u32;
        self.opaque.push(Op {
            kind: op.kind.clone(),
            operands: op.operands.clone(),
            regions,
            // An opaque op is reproduced verbatim at extraction, so its
            // declared type must survive the round trip.
            ty: op.ty,
        });
        Some(self.add(ENode {
            kind: NodeKind::Opaque(serial),
            children,
            ty: op.ty,
        }))
    }

    // ---- saturation --------------------------------------------------------

    /// Applies every rewrite rule repeatedly until nothing new is
    /// discovered or a limit is hit.
    pub(crate) fn saturate(&mut self, opt: &EGraphOptimizer) -> SaturationReport {
        let mut iterations = 0;
        let mut saturated = false;

        for _ in 0..opt.max_iterations {
            iterations += 1;
            // Collect matches against a *snapshot*, then apply: mutating
            // while matching would make rule application order-dependent.
            let matches = crate::rules::collect_matches(self);
            if matches.is_empty() {
                saturated = true;
                break;
            }
            let mut changed = false;
            for (class, term) in matches {
                // A rewrite is an *equality*, so the replacement has the
                // same type as the class it rewrites. Nested subterms
                // inherit it, which is correct for every rule in the set:
                // arithmetic/bitwise/shift rules are same-type throughout,
                // and the comparison rules only reference existing classes.
                let ty = self.class_ty(class);
                let new_class = crate::rules::instantiate(self, &term, ty);
                changed |= self.union(class, new_class);
            }
            self.rebuild();
            if !changed {
                saturated = true;
                break;
            }
            if self.num_nodes() >= opt.max_nodes {
                break;
            }
        }

        SaturationReport {
            iterations,
            nodes: self.num_nodes(),
            classes: self.num_classes(),
            saturated,
        }
    }
}

/// Whether an op is a pure, region-free computation eligible for
/// hashconsing and rewriting.
pub(crate) fn is_pure(kind: &OpKind) -> bool {
    matches!(
        kind,
        OpKind::Const(_)
            | OpKind::Add
            | OpKind::Sub
            | OpKind::Mul
            | OpKind::Div
            | OpKind::Rem
            | OpKind::Neg
            | OpKind::Eq
            | OpKind::Ne
            | OpKind::Lt
            | OpKind::Le
            | OpKind::Gt
            | OpKind::Ge
            | OpKind::And
            | OpKind::Or
            | OpKind::Not
            | OpKind::BitAnd
            | OpKind::BitOr
            | OpKind::BitXor
            | OpKind::Shl
            | OpKind::Shr
            | OpKind::Tuple
            | OpKind::TupleGet(_)
            | OpKind::Arg(_)
            | OpKind::BlockArg(_)
            | OpKind::ParamRef(_)
    )
}
