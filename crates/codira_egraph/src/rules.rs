//! Copyright (c) 2026 Omnira CJSC. All Rights Reserved.
//! Author: Tunjay Akbarli
//! Date: September 15, 2026
//!
//! The rewrite rules.
//!
//! Each rule is a direct pattern match over one e-node, producing a
//! [`Term`] -- a small tree naming the equivalent expression. Rules are
//! hand-written against the typed [`OpKind`] rather than parsed from a
//! string DSL: the rule set is fixed, and a typed match is both faster and
//! impossible to misspell.
//!
//! # Why a `Term` and not an `ENode`
//!
//! Matching borrows the graph immutably (so that rule application never
//! depends on the order classes are visited in), but a rule like
//! `x * 8 == x << 3` must *introduce* a literal `3` that may not exist in
//! the graph yet. So a rule returns a term tree, and the applier
//! instantiates it afterwards -- adding any missing constant classes as it
//! goes. This is exactly how egg separates pattern matching from pattern
//! instantiation.
//!
//! # Two rule tiers
//!
//! **Ungated** rules hold for every type `fold_op` admits, including IEEE
//! floats: commutativity of the commutative operators, boolean
//! idempotence, double negation, comparison mirroring. These are always
//! safe.
//!
//! **Int-gated** rules restructure arithmetic and therefore require
//! [`definitely_int`](crate::egraph::Analysis) on the operands. Float `+`
//! and `*` are commutative but **not associative** (rounding makes
//! `(a+b)+c != a+(b+c)` in general), and `x * 0 == 0` is false for
//! `x = NaN` or `x = inf`. Gating is a soundness requirement, not a
//! tuning knob -- see the crate doc.
//!
//! The split between the tiers is drawn *per identity*, not per operator,
//! because IEEE semantics do not respect operator boundaries. The
//! instructive pair:
//!
//! * `x - 0 == x` is **exact for every float**: subtracting `+0.0` returns `x`
//!   unchanged, including for `x = -0.0` (`-0.0 - 0.0` is `-0.0`) and for NaN.
//! * `x + 0 == x` is **false for `x = -0.0`**: `-0.0 + 0.0` is `+0.0`, a
//!   different value with a different sign bit.
//!
//! So the subtraction identity is ungated and the addition identity is
//! gated, even though the two look symmetric. `x * 1` and `x / 1` are
//! likewise exact for all floats (multiplication and division by one
//! introduce no rounding), while `x * 0` is not. These are the same
//! distinctions LLVM draws between its plain and `fast-math` rewrites,
//! and that Alive2 exists to police.
//!
//! # Why both directions of distributivity
//!
//! `a*b + a*c <-> a*(b+c)` is registered in *both* directions. In a
//! destructive optimizer that would loop forever; in an e-graph it simply
//! records that the two forms are equal, and the cost model decides which
//! to extract. Factoring is usually cheaper (one multiply instead of two),
//! but expanding can expose a constant fold the factored form hides. This
//! is exactly the phase-ordering freedom equality saturation buys.

use codira_mir::{Attr, OpKind, TypeId};
use smallvec::SmallVec;

use crate::egraph::{EClassId, EGraph, ENode, NodeKind};

/// The right-hand side of a rule: a tree over existing classes, fresh
/// constants, and operations, instantiated into the graph by
/// [`instantiate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Term {
    /// An already-existing e-class (a subterm the rule matched).
    Class(EClassId),
    /// A literal, added to the graph if it is not present.
    Const(Attr),
    /// An operation over sub-terms.
    Node(OpKind, Vec<Term>),
}

/// A rewrite to apply: the matched class is equal to this term.
pub(crate) type Match = (EClassId, Term);

/// Materializes a [`Term`] into the graph, returning its class.
/// `ty` is the type of the term being built. Sub-terms inherit it, which
/// is correct for every rule currently in the set (arithmetic, bitwise and
/// shift rules are same-type throughout; the comparison rules only
/// reference pre-existing classes rather than building new typed nodes).
/// A future rule that changes type between a term and its sub-terms must
/// carry explicit types on the pattern.
pub(crate) fn instantiate(graph: &mut EGraph, term: &Term, ty: TypeId) -> EClassId {
    match term {
        Term::Class(id) => graph.find(*id),
        Term::Const(attr) => graph.add(ENode {
            kind: NodeKind::Pure(OpKind::Const(attr.clone())),
            children: SmallVec::default(),
            ty,
        }),
        Term::Node(kind, args) => {
            let children = args.iter().map(|a| instantiate(graph, a, ty)).collect();
            graph.add(ENode {
                kind: NodeKind::Pure(kind.clone()),
                children,
                ty,
            })
        }
    }
}

/// Scans every class for rule matches. Read-only: the caller instantiates
/// and unions the results, so rule application never depends on the order
/// classes happen to be visited in.
pub(crate) fn collect_matches(graph: &EGraph) -> Vec<Match> {
    let mut out = Vec::new();
    for class in graph.class_ids() {
        for node in graph.nodes(class) {
            let NodeKind::Pure(kind) = &node.kind else {
                continue;
            };
            match_node(graph, class, kind, &node.children, &mut out);
        }
    }
    // Several rules can produce the same conclusion; deduping keeps the
    // "nothing new this round" fixpoint check honest.
    out.sort_by_key(|a| a.0);
    out.dedup();
    out
}

fn is_int(graph: &EGraph, id: EClassId) -> bool {
    graph.analysis(id).is_some_and(|a| a.definitely_int)
}

fn const_int(graph: &EGraph, id: EClassId) -> Option<i64> {
    match graph.analysis(id).and_then(|a| a.constant.clone()) {
        Some(Attr::Int(v)) => Some(v),
        _ => None,
    }
}

/// The first node in `id`'s class whose operator satisfies `pred`, with
/// its children -- the e-graph equivalent of destructuring a subterm.
fn find_op(graph: &EGraph, id: EClassId, pred: impl Fn(&OpKind) -> bool) -> Option<Vec<EClassId>> {
    graph.nodes(id).iter().find_map(|n| match &n.kind {
        NodeKind::Pure(k) if pred(k) => Some(n.children.to_vec()),
        _ => None,
    })
}

fn zero() -> Term {
    Term::Const(Attr::Int(0))
}

fn match_node(
    graph: &EGraph,
    class: EClassId,
    kind: &OpKind,
    children: &[EClassId],
    out: &mut Vec<Match>,
) {
    use OpKind::{
        Add, And, BitAnd, BitOr, BitXor, Div, Eq, Ge, Gt, Le, Lt, Mul, Ne, Neg, Not, Or, Shl, Shr,
        Sub,
    };
    let mut push = |term: Term| out.push((class, term));

    // ================= ungated (sound for every type) =================

    // Commutativity. Holds for IEEE floats as well as integers.
    if matches!(
        kind,
        Add | Mul | And | Or | BitAnd | BitOr | BitXor | Eq | Ne
    ) {
        if let [a, b] = children {
            if a != b {
                push(Term::Node(
                    kind.clone(),
                    vec![Term::Class(*b), Term::Class(*a)],
                ));
            }
        }
    }

    // Comparison mirroring: `a > b` is `b < a`. A pure operand swap, no
    // arithmetic restructuring, so no type gate is needed. Registering
    // both directions lets the cost model settle on one canonical form.
    if let [a, b] = children {
        let mirrored = match kind {
            Gt => Some(Lt),
            Lt => Some(Gt),
            Ge => Some(Le),
            Le => Some(Ge),
            _ => None,
        };
        if let Some(op) = mirrored {
            push(Term::Node(op, vec![Term::Class(*b), Term::Class(*a)]));
        }
    }

    match kind {
        // x && x == x, x || x == x. Booleans have no NaN subtlety.
        And | Or => {
            if let [a, b] = children {
                if a == b {
                    push(Term::Class(*a));
                }
            }
        }
        // !!x == x
        Not => {
            if let [a] = children {
                if let Some(inner) = find_op(graph, *a, |k| matches!(k, Not)) {
                    if let [x] = inner.as_slice() {
                        push(Term::Class(*x));
                    }
                }
            }
        }
        // --x == x. Exact for floats too: negation only flips the sign
        // bit, so it is involutive with no rounding.
        Neg => {
            if let [a] = children {
                if let Some(inner) = find_op(graph, *a, |k| matches!(k, Neg)) {
                    if let [x] = inner.as_slice() {
                        push(Term::Class(*x));
                    }
                }
            }
        }
        // !(a == b) == (a != b), and the converse.
        _ => {}
    }
    if let [a] = children {
        if matches!(kind, Not) {
            if let Some(inner) = find_op(graph, *a, |k| matches!(k, Eq)) {
                if let [x, y] = inner.as_slice() {
                    push(Term::Node(Ne, vec![Term::Class(*x), Term::Class(*y)]));
                }
            }
        }
    }
    if matches!(kind, Ne) {
        if let [a, b] = children {
            push(Term::Node(
                Not,
                vec![Term::Node(Eq, vec![Term::Class(*a), Term::Class(*b)])],
            ));
        }
    }

    // ================= int-gated (unsound for floats) =================

    match kind {
        Add => {
            if let [a, b] = children {
                // x + 0 == x
                if const_int(graph, *b) == Some(0) && is_int(graph, *a) {
                    push(Term::Class(*a));
                }
                // x + x == x << 1
                if a == b && is_int(graph, *a) {
                    push(Term::Node(
                        Shl,
                        vec![Term::Class(*a), Term::Const(Attr::Int(1))],
                    ));
                }
                // Associativity: (x + y) + z == x + (y + z)
                if is_int(graph, *a) && is_int(graph, *b) {
                    if let Some(inner) = find_op(graph, *a, |k| matches!(k, Add)) {
                        if let [x, y] = inner.as_slice() {
                            push(Term::Node(
                                Add,
                                vec![
                                    Term::Class(*x),
                                    Term::Node(Add, vec![Term::Class(*y), Term::Class(*b)]),
                                ],
                            ));
                        }
                    }
                }
                // Factoring: a*b + a*c == a*(b + c). Both multiplications
                // must share an operand; commutativity (already a rule)
                // supplies the other orientations, so matching the two
                // canonical positions here is enough.
                if is_int(graph, *a) && is_int(graph, *b) {
                    if let (Some(l), Some(r)) = (
                        find_op(graph, *a, |k| matches!(k, Mul)),
                        find_op(graph, *b, |k| matches!(k, Mul)),
                    ) {
                        if let ([la, lb], [ra, rb]) = (l.as_slice(), r.as_slice()) {
                            // Find a shared factor and the two cofactors.
                            let shared = if la == ra {
                                Some((*la, *lb, *rb))
                            } else if la == rb {
                                Some((*la, *lb, *ra))
                            } else if lb == ra {
                                Some((*lb, *la, *rb))
                            } else if lb == rb {
                                Some((*lb, *la, *ra))
                            } else {
                                None
                            };
                            if let Some((common, x, y)) = shared {
                                push(Term::Node(
                                    Mul,
                                    vec![
                                        Term::Class(common),
                                        Term::Node(Add, vec![Term::Class(x), Term::Class(y)]),
                                    ],
                                ));
                            }
                        }
                    }
                }
            }
        }

        Sub => {
            if let [a, b] = children {
                // x - 0 == x. UNGATED: exact for every float, including
                // -0.0 and NaN (see the module doc's comparison with the
                // addition identity).
                if const_int(graph, *b) == Some(0) {
                    push(Term::Class(*a));
                }
                // x - x == 0
                if a == b && is_int(graph, *a) {
                    push(zero());
                }
                // (x + y) - y == x   and   (x + y) - x == y
                if let Some(inner) = find_op(graph, *a, |k| matches!(k, Add)) {
                    if let [x, y] = inner.as_slice() {
                        if y == b && is_int(graph, *x) {
                            push(Term::Class(*x));
                        }
                        if x == b && is_int(graph, *y) {
                            push(Term::Class(*y));
                        }
                    }
                }
            }
        }

        Mul => {
            if let [a, b] = children {
                // x * 1 == x. UNGATED: multiplication by one is exact in
                // IEEE (no rounding, sign and NaN preserved).
                if const_int(graph, *b) == Some(1) {
                    push(Term::Class(*a));
                }
                // x * 0 == 0. FALSE for float NaN/inf -- hence the gate.
                if const_int(graph, *b) == Some(0) && is_int(graph, *a) {
                    push(zero());
                }
                // Strength reduction: x * 2^k == x << k
                if is_int(graph, *a) {
                    if let Some(c) = const_int(graph, *b) {
                        // `is_power_of_two` is unsigned-only; `c >= 2`
                        // makes the cast lossless.
                        if c >= 2 && (c as u64).is_power_of_two() {
                            let k = i64::from(c.trailing_zeros());
                            push(Term::Node(
                                Shl,
                                vec![Term::Class(*a), Term::Const(Attr::Int(k))],
                            ));
                        }
                    }
                }
                // Associativity: (x * y) * z == x * (y * z)
                if is_int(graph, *a) && is_int(graph, *b) {
                    if let Some(inner) = find_op(graph, *a, |k| matches!(k, Mul)) {
                        if let [x, y] = inner.as_slice() {
                            push(Term::Node(
                                Mul,
                                vec![
                                    Term::Class(*x),
                                    Term::Node(Mul, vec![Term::Class(*y), Term::Class(*b)]),
                                ],
                            ));
                        }
                    }
                }
                // Expansion: a*(b + c) == a*b + a*c. The converse of the
                // factoring rule above -- see the module doc.
                if is_int(graph, *a) && is_int(graph, *b) {
                    if let Some(inner) = find_op(graph, *b, |k| matches!(k, Add)) {
                        if let [x, y] = inner.as_slice() {
                            push(Term::Node(
                                Add,
                                vec![
                                    Term::Node(Mul, vec![Term::Class(*a), Term::Class(*x)]),
                                    Term::Node(Mul, vec![Term::Class(*a), Term::Class(*y)]),
                                ],
                            ));
                        }
                    }
                }
            }
        }

        Div => {
            if let [a, b] = children {
                // x / 1 == x. UNGATED: division by one is exact in IEEE,
                // for the same reason as multiplication by one.
                if const_int(graph, *b) == Some(1) {
                    push(Term::Class(*a));
                }
            }
        }

        BitAnd | BitOr => {
            if let [a, b] = children {
                // x & x == x, x | x == x
                if a == b {
                    push(Term::Class(*a));
                }
                // x | 0 == x, x & -1 == x
                let identity = if matches!(kind, BitOr) { 0 } else { -1 };
                if const_int(graph, *b) == Some(identity) {
                    push(Term::Class(*a));
                }
            }
        }

        BitXor => {
            if let [a, b] = children {
                // x ^ x == 0
                if a == b {
                    push(zero());
                }
                // x ^ 0 == x
                if const_int(graph, *b) == Some(0) {
                    push(Term::Class(*a));
                }
            }
        }

        Shl | Shr => {
            if let [a, b] = children {
                // x << 0 == x, x >> 0 == x
                if const_int(graph, *b) == Some(0) {
                    push(Term::Class(*a));
                }
                // (x << a) << b == x << (a + b), when a + b stays in range
                // (fold_op rejects shift amounts >= 64 as an evaluation
                // error, so a merged shift must not cross that line).
                if matches!(kind, Shl) {
                    if let (Some(outer), Some(inner)) = (
                        const_int(graph, *b),
                        find_op(graph, *a, |k| matches!(k, Shl)),
                    ) {
                        if let [x, k] = inner.as_slice() {
                            if let Some(inner_k) = const_int(graph, *k) {
                                if inner_k + outer < 64 && inner_k >= 0 && outer >= 0 {
                                    push(Term::Node(
                                        Shl,
                                        vec![
                                            Term::Class(*x),
                                            Term::Const(Attr::Int(inner_k + outer)),
                                        ],
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        _ => {}
    }
}
