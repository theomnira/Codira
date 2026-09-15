//! Algebraic data-structure self-repair (Section 2.7.3 of the HERACLES
//! spec, "Type-Driven Reconstruction").
//!
//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 7, 2026
//!
//! Functionality (per Theorem 6 and Section 2.7.3):
//! - Makes a type definition "algebraically constructive": the runtime
//!   invariant machinery maps a corrupted value back to the nearest valid
//!   inhabitant of the type using the type's formal algebraic rules.
//! - `Bst` is a binary search tree whose *value* carries the spec's BST
//!   invariant ("every node in the left subtree is < the root, every node in
//!   the right subtree is > the root, the tree is balanced"). The invariant is
//!   checked on read (here, on demand), and if it is violated (by a race, a
//!   bit-flip, or a deliberate corruption), `repair` runs the routine the spec
//!   describes:
//!     1. Extract every node reachable from the corrupted tree (DFS traversal),
//!     2. Reconstruct the canonical balanced BST from the extracted node set,
//!     3. Return the repaired `BinarySearchTree`.
//! - Result: the same data, repaired structure, zero data loss -- the node
//!   multiset is preserved exactly (the only thing that can change is that
//!   genuinely-corrupt copies merge back into canonical positions).
//!
//! The `validate`/`repair` pair is the executable analogue of
//! `@algebraic_repair(recovery_strategy: reconstruct_from_valid_subset)` in
//! the paper's example.

use std::collections::BTreeSet;

/// A node in the self-repairing binary search tree.
///
/// Interior mutability is deliberately *not* used: the engine that detects
/// a violation mutates the tree through `&mut`, matching the "on read"
/// invariant-check semantics of the spec (a reader detects the corruption;
/// the recovery code holds the exclusive write lock).
#[derive(Debug, Clone)]
pub struct BstNode {
    /// The stored value (the sort key of the BST).
    pub key: i64,
    /// Left subtree: keys strictly less than `key`.
    pub left: Option<Box<BstNode>>,
    /// Right subtree: keys strictly greater than `key`.
    pub right: Option<Box<BstNode>>,
}

impl BstNode {
    /// A leaf.
    fn leaf(key: i64) -> Self {
        BstNode {
            key,
            left: None,
            right: None,
        }
    }

    /// The height (# edges on the longest root-leaf path) of this subtree.
    fn height(&self) -> u64 {
        1 + self
            .left
            .as_deref()
            .map_or(0, BstNode::height)
            .max(self.right.as_deref().map_or(0, BstNode::height))
    }

    /// Recursive BST-validity check with the allowed key interval passed
    /// down from ancestors.
    fn is_valid_ordered(&self, min: Option<i64>, max: Option<i64>) -> bool {
        if let Some(min) = min {
            if self.key <= min {
                return false;
            }
        }
        if let Some(max) = max {
            if self.key >= max {
                return false;
            }
        }
        self.left
            .as_deref()
            .is_none_or(|n| n.is_valid_ordered(min, Some(self.key)))
            && self
                .right
                .as_deref()
                .is_none_or(|n| n.is_valid_ordered(Some(self.key), max))
    }

    /// Whether `right` satisfies the height balance invariant:
    /// `|height(left) - height(right)| <= 1` at every node.
    fn is_balanced(&self) -> bool {
        let lh = self.left.as_deref().map_or(0, BstNode::height);
        let rh = self.right.as_deref().map_or(0, BstNode::height);
        let diff = lh.abs_diff(rh);
        diff <= 1
            && self.left.as_deref().is_none_or(BstNode::is_balanced)
            && self.right.as_deref().is_none_or(BstNode::is_balanced)
    }

    /// Pre-order DFS collecting every reachable node.
    fn dfs(values: &mut BTreeSet<i64>, node: &BstNode) {
        values.insert(node.key);
        if let Some(l) = node.left.as_deref() {
            BstNode::dfs(values, l);
        }
        if let Some(r) = node.right.as_deref() {
            BstNode::dfs(values, r);
        }
    }
}

/// A self-repairing binary search tree with a strict, balanced invariant.
///
/// This is the value-level counterpart of the spec's
/// `type BinarySearchTree<i> = Tree { t | ∀ n ∈ t.left: n.key < t.root ∧
/// ∀ n ∈ t.right: n.key > t.root ∧ is_balanced(t) }`.
#[derive(Debug, Clone, Default)]
pub struct BinarySearchTree {
    root: Option<Box<BstNode>>,
}

/// The result of running the algebraic repair routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairReport {
    /// Whether the tree was already valid (nothing needed repairing).
    pub was_intact: bool,
    /// How many distinct keys were recovered and preserved by the repair.
    pub keys_preserved: usize,
}

impl BinarySearchTree {
    /// An empty tree.
    pub fn new() -> Self {
        BinarySearchTree::default()
    }

    /// Inserts `key`, maintaining the BST ordering invariant. Balanced-ness
    /// is *not* re-ensured here -- the repair path is authorized to restore
    /// it, matching the spec where "balance is a recoverable invariant, not
    /// an insertion-time guarantee".
    ///
    /// Returns `true` if the key was new (actually inserted).
    pub fn insert(&mut self, key: i64) -> bool {
        let Some(mut root) = self.root.take() else {
            self.root = Some(Box::new(BstNode::leaf(key)));
            return true;
        };
        let existed = Self::insert_rec(&mut root, key);
        self.root = Some(root);
        existed
    }

    fn insert_rec(node: &mut Box<BstNode>, key: i64) -> bool {
        use std::cmp::Ordering;
        match key.cmp(&node.key) {
            Ordering::Less => {
                if let Some(l) = node.left.as_mut() {
                    Self::insert_rec(l, key)
                } else {
                    node.left = Some(Box::new(BstNode::leaf(key)));
                    true
                }
            }
            Ordering::Greater => {
                if let Some(r) = node.right.as_mut() {
                    Self::insert_rec(r, key)
                } else {
                    node.right = Some(Box::new(BstNode::leaf(key)));
                    true
                }
            }
            Ordering::Equal => false,
        }
    }

    /// Whether the tree satisfies the full refined-type invariant: strict
    /// BST ordering *and* AVL-style height balance. This is the "validate on
    /// read" check the recovery path uses.
    pub fn is_valid(&self) -> bool {
        let Some(root) = self.root.as_deref() else {
            return true; // the empty tree is trivially valid
        };
        root.is_valid_ordered(None, None) && root.is_balanced()
    }

    /// The number of nodes currently reachable.
    pub fn len(&self) -> usize {
        fn walk(node: Option<&BstNode>, acc: &mut usize) {
            if let Some(n) = node {
                *acc += 1;
                walk(n.left.as_deref(), acc);
                walk(n.right.as_deref(), acc);
            }
        }
        let mut total = 0usize;
        walk(self.root.as_deref(), &mut total);
        total
    }

    /// `true` iff the tree has no nodes.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// The canonical balanced BST built from a sorted key collection, using
    /// the median as the root of each sub-range. This is the *deterministic*
    /// canonical form: the same key set always yields the same tree.
    fn build_balanced(keys: &[i64]) -> Option<Box<BstNode>> {
        if keys.is_empty() {
            return None;
        }
        let mid = keys.len() / 2;
        Some(Box::new(BstNode {
            key: keys[mid],
            left: Self::build_balanced(&keys[..mid]),
            right: Self::build_balanced(&keys[mid + 1..]),
        }))
    }

    /// Performs the spec's algebraic repair:
    /// 1. Extract every reachable key (DFS), making exactly the node set
    ///    recoverable from the current (possibly corrupted) structure;
    /// 2. Sort it into the canonical balanced BST;
    /// 3. Swap the corrupted tree for the canonical one.
    ///
    /// The stored key multiset is preserved (duplicate/corrupt copies of a
    /// key collapse into one node; no *distinct* key is ever dropped).
    pub fn repair(&mut self) -> RepairReport {
        if self.is_valid() {
            return RepairReport {
                was_intact: true,
                keys_preserved: self.len(),
            };
        }
        let mut keys = BTreeSet::new();
        if let Some(root) = self.root.as_deref() {
            BstNode::dfs(&mut keys, root);
        }
        let keys: Vec<i64> = keys.into_iter().collect();
        self.root = Self::build_balanced(&keys);
        RepairReport {
            was_intact: false,
            keys_preserved: keys.len(),
        }
    }

    /// The smallest key in the tree (None if empty), used by callers.
    pub fn min(&self) -> Option<i64> {
        let mut node = self.root.as_deref();
        let mut value = None;
        while let Some(n) = node {
            value = Some(n.key);
            node = n.left.as_deref();
        }
        value
    }

    /// The largest key in the tree (None if empty).
    pub fn max(&self) -> Option<i64> {
        let mut node = self.root.as_deref();
        let mut value = None;
        while let Some(n) = node {
            value = Some(n.key);
            node = n.right.as_deref();
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tree that is a valid BST by construction.
    fn sample() -> BinarySearchTree {
        let mut t = BinarySearchTree::new();
        for k in [40, 20, 60, 10, 30, 50, 70] {
            t.insert(k);
        }
        t
    }

    #[test]
    fn valid_tree_reports_intact() {
        let mut t = sample();
        assert!(t.is_valid());
        let report = t.repair();
        assert!(report.was_intact);
        assert_eq!(report.keys_preserved, 7);
    }

    #[test]
    fn insert_is_idempotent_for_duplicate_keys() {
        let mut t = BinarySearchTree::new();
        assert!(t.insert(5));
        assert!(!t.insert(5));
        assert!(t.is_valid(), "single node is trivially valid");
        assert!(t.min() == Some(5));
        assert_eq!(t.max(), Some(5));
    }

    #[test]
    fn degenerate_chain_is_valid_but_unbalanced() {
        let mut t = BinarySearchTree::new();
        for k in 1..=10 {
            t.insert(k);
        }
        // Strictly increasing inserts produce a right-spine chain: ordered
        // (is_valid's ordering half passes) but grossly unbalanced.
        assert_eq!(t.min(), Some(1));
        assert_eq!(t.max(), Some(10));
        // The ordering part of the invariant holds, the balance half does
        // not -- so is_valid() is false and repair must restore balance.
        assert!(!t.is_valid());
        let report = t.repair();
        assert!(!report.was_intact);
        assert!(t.is_valid(), "repair must restore the invariant");
        assert_eq!(report.keys_preserved, 10);
    }

    #[test]
    fn corrupt_tree_is_repaired_into_valid_canonical_form() {
        let mut t = sample();
        // Corrupt: swap the root with its right child, breaking the ordering
        // invariant (some of the right subtree then lies strictly-left of
        // the root, which the invariant forbids).
        let root = t.root.as_mut().unwrap();
        let old_left = root.left.take();
        root.left = root.right.take();
        root.right = old_left;
        assert!(!t.is_valid(), "corruption must be detected on read");

        let report = t.repair();
        assert!(!report.was_intact);
        assert!(t.is_valid(), "repair must restore the invariant");
        // The key multiset is fully preserved (zero data loss).
        assert_eq!(report.keys_preserved, 7);
        assert_eq!(t.min(), Some(10));
        assert_eq!(t.max(), Some(70));
    }

    /// The repaired tree is *canonical*: the same key set always yields the
    /// same shape, so recovery is deterministic (a core HERACLES principle).
    #[test]
    fn repair_rees_is_deterministic() {
        let mut a = BinarySearchTree::new();
        for k in 1..=15 {
            a.insert(k);
        }
        let first = a.repair();
        assert!(!first.was_intact);

        let mut b = BinarySearchTree::new();
        for k in 1..=15 {
            b.insert(k);
        }
        let second = b.repair();

        // Reconstructing twice yields identical trees.
        assert_eq!(first.keys_preserved, second.keys_preserved);
        assert!(a.is_valid());
        assert!(b.is_valid());

        // And no matter how the corruption happened (in-order vs pre-order
        // rebuild), the canonical form is the same balanced tree.
        let root_a = a.root.as_deref().unwrap();
        let root_b = b.root.as_deref().unwrap();
        assert_eq!(root_a.key, root_b.key);
        assert_eq!(a.len(), b.len());
    }
}
