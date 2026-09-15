//! Copyright (c) 2026 Omnira CJSC
//!
//! Algebraic canonicalization: identity/annihilator/idempotence rewrites
//! and a little strength reduction. The peephole layer of the pipeline --
//! KGEN analog: the canonicalization patterns that run interleaved with
//! folding in `modular/KGEN/lib/Transforms/` (MLIR `canonicalize`), kept
//! deliberately much smaller than an e-graph: these are only the rewrites
//! that are (a) unconditionally profitable and (b) provable without type
//! information.
//!
//! # Safety model
//!
//! Operand *effects* are a non-issue: every value-producing op except
//! `core.call` and the region ops is pure, and the rules below never drop
//! a use of a call or a region op (calls/loops/ifs are opaque to this
//! pass), so dropping a use is always effect-safe. What needs care is
//! **numeric semantics without types**: `Attr` untyped operands may turn
//! out to be floats at runtime, and some classic identities are float-lies.
//! The policy, per rule class:
//!
//! * Applied unconditionally (exact for ints; for floats either exact or
//!   accepted-by-design, noted inline): `x+0`, `0+x`, `x-0` (float-exact except
//!   that `-0.0 + 0` gives `+0.0` -- accepted: the surface language has no
//!   negative-zero literal semantics to preserve), `x*1`, `1*x`, `x/1`
//!   (float-exact), `x*0`/`0*x -> 0` (a float NaN/inf operand would
//!   legitimately produce NaN -- accepted, and noted here honestly rather than
//!   hidden), `x&x`, `x|x`, `x^x -> 0` (bitwise ops are int-only by `fold_op`),
//!   `x&&x`, `x||x` (bool-only), `!!x`, `--x` (exact for ints and IEEE floats
//!   alike), `shl/shr by 0`.
//! * Applied **only under [`definitely_int`]** (a NaN operand breaks all three,
//!   and floats break the shifts): `x-x -> 0`, `x==x -> true`, `x!=x -> false`,
//!   `x*2^k -> x shl k` (k from a `const int >= 2`), `x+x -> x shl 1`.
//!   `definitely_int` is a conservative syntactic proof: bitwise/shift ops are
//!   int-only by `fold_op`'s semantics, `const int` is int, and int-in/int-out
//!   arithmetic (`add`/`sub`/ `mul`/`div`/`rem`/`neg` -- these promote only
//!   when a float operand is present) propagates the proof;
//!   `Arg`/`BlockArg`/`Call`/ `ParamRef`/everything else is unknown, i.e.
//!   `false`. When the proof fails the rule is simply skipped -- missing an
//!   optimization is fine, changing float semantics is not.
//!
//! Like every rebuilding pass here, use-forwarding rules (`x*1 -> x`) are
//! suppressed for the body's *result* op: the result is positional (last
//! op), so forwarding it would reassign the result. Value-producing
//! rewrites (`x^x -> const 0`, strength reduction) emit a new op at the
//! same position and are safe anywhere. The pass assumes verifier-clean,
//! type-correct input (a `shl` whose operand is secretly a string would
//! fail at runtime either way; simplifying it merely fails later).

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{
    rewrite::{const_int, remap_operands},
    Changed, Pass, PassContext,
};
use crate::{
    op::{Body, OpId, OpKind, Region},
    Attr,
};

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct SimplifyPass;

impl Pass for SimplifyPass {
    fn name(&self) -> &'static str {
        "simplify"
    }

    fn run(&self, body: &mut Body, _ctx: &PassContext<'_>) -> Changed {
        let (new, changed) = rewrite_body(body);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// Conservative syntactic proof that `id`'s value is an integer -- see
/// the module doc. Memoized (the operand graph is a DAG; naive recursion
/// would be exponential on diamond shapes).
fn definitely_int(body: &Body, id: OpId, memo: &mut FxHashMap<OpId, bool>) -> bool {
    if let Some(&known) = memo.get(&id) {
        return known;
    }
    let op = body.get(id);
    // Several arms answer `true` for different documented reasons (a
    // literal is an int; bitwise ops are int-only by fold_op). Merging
    // them would erase why.
    #[allow(clippy::match_same_arms)]
    let proven = match &op.kind {
        OpKind::Const(Attr::Int(_)) => true,
        // Int-only by `fold_op`: these reject float operands outright.
        OpKind::BitAnd | OpKind::BitOr | OpKind::BitXor | OpKind::Shl | OpKind::Shr => true,
        // Int-preserving when all inputs are ints (`fold_op` promotes to
        // float only when a float operand is present).
        OpKind::Neg => definitely_int(body, op.operands[0], memo),
        OpKind::Add | OpKind::Sub | OpKind::Mul | OpKind::Div | OpKind::Rem => {
            let operands: SmallVec<[OpId; 2]> = op.operands.clone();
            operands.iter().all(|&o| definitely_int(body, o, memo))
        }
        _ => false,
    };
    memo.insert(id, proven);
    proven
}

/// One decided rewrite for an op (operands already renumbered into the
/// new body).
enum Rw {
    /// Replace all uses with an existing value; drop the def.
    Forward(OpId),
    /// Replace the op with `core.const(attr)` at the same position.
    Out(Attr),
    /// Replace the op with `x shl amount` (strength reduction).
    ShlBy(OpId, i64),
}

fn is_const_int(body: &Body, id: OpId, v: i64) -> bool {
    const_int(body, id) == Some(v)
}

#[allow(clippy::too_many_lines)] // one arm per rule; splitting would obscure the catalog
fn decide(
    new: &Body,
    kind: &OpKind,
    ops: &[OpId],
    is_result: bool,
    memo: &mut FxHashMap<OpId, bool>,
) -> Option<Rw> {
    use OpKind::{Add, And, BitAnd, BitOr, BitXor, Div, Eq, Mul, Ne, Neg, Not, Or, Shl, Shr, Sub};
    let fwd = |v: OpId| {
        if is_result {
            None // forwarding the positional result is never safe
        } else {
            Some(Rw::Forward(v))
        }
    };
    match kind {
        Add => {
            let (a, b) = (ops[0], ops[1]);
            if is_const_int(new, b, 0) {
                return fwd(a);
            }
            if is_const_int(new, a, 0) {
                return fwd(b);
            }
            if a == b && definitely_int(new, a, memo) {
                return Some(Rw::ShlBy(a, 1)); // x + x -> x shl 1
            }
            None
        }
        Sub => {
            let (a, b) = (ops[0], ops[1]);
            if is_const_int(new, b, 0) {
                return fwd(a);
            }
            if a == b && definitely_int(new, a, memo) {
                return Some(Rw::Out(Attr::Int(0))); // x - x -> 0 (int only)
            }
            None
        }
        Mul => {
            let (a, b) = (ops[0], ops[1]);
            if is_const_int(new, b, 1) {
                return fwd(a);
            }
            if is_const_int(new, a, 1) {
                return fwd(b);
            }
            if is_const_int(new, a, 0) || is_const_int(new, b, 0) {
                return Some(Rw::Out(Attr::Int(0))); // float NaN caveat: module
                                                    // doc
            }
            for (x, c) in [(a, b), (b, a)] {
                if let Some(k) = const_int(new, c) {
                    if k >= 2 && (k as u64).is_power_of_two() && definitely_int(new, x, memo) {
                        return Some(Rw::ShlBy(x, i64::from(k.trailing_zeros())));
                    }
                }
            }
            None
        }
        Div => {
            let (a, b) = (ops[0], ops[1]);
            if is_const_int(new, b, 1) {
                return fwd(a);
            }
            None
        }
        And | Or | BitAnd | BitOr => {
            let (a, b) = (ops[0], ops[1]);
            if a == b {
                return fwd(a); // idempotence
            }
            None
        }
        BitXor => {
            let (a, b) = (ops[0], ops[1]);
            if a == b {
                return Some(Rw::Out(Attr::Int(0))); // int-only op: always safe
            }
            None
        }
        Eq | Ne => {
            let (a, b) = (ops[0], ops[1]);
            // Same SSA value *and* provably int (floats: NaN != NaN).
            if a == b && definitely_int(new, a, memo) {
                return Some(Rw::Out(Attr::Bool(matches!(kind, Eq))));
            }
            None
        }
        Shl | Shr => {
            let (a, b) = (ops[0], ops[1]);
            if is_const_int(new, b, 0) {
                return fwd(a);
            }
            None
        }
        Not => {
            let inner = new.get(ops[0]);
            if matches!(inner.kind, Not) {
                return fwd(inner.operands[0]); // !!x -> x
            }
            None
        }
        Neg => {
            let inner = new.get(ops[0]);
            if matches!(inner.kind, Neg) {
                return fwd(inner.operands[0]); // --x -> x (IEEE-exact too)
            }
            None
        }
        _ => None,
    }
}

fn rewrite_body(src: &Body) -> (Body, bool) {
    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut memo: FxHashMap<OpId, bool> = FxHashMap::default();
    let mut changed = false;
    let last = src.result();

    for (id, op) in src.iter() {
        let operands = remap_operands(op, &map);
        if op.regions.is_empty() {
            match decide(&new, &op.kind, &operands, Some(id) == last, &mut memo) {
                Some(Rw::Forward(v)) => {
                    map.insert(id, v);
                    changed = true;
                    continue;
                }
                Some(Rw::Out(attr)) => {
                    map.insert(id, new.push(OpKind::Const(attr), []));
                    changed = true;
                    continue;
                }
                Some(Rw::ShlBy(x, amount)) => {
                    let amount = new.push(OpKind::Const(Attr::Int(amount)), []);
                    map.insert(id, new.push(OpKind::Shl, [x, amount]));
                    changed = true;
                    continue;
                }
                None => {}
            }
        }

        let regions: SmallVec<[Region; 0]> = op
            .regions
            .iter()
            .map(|r| {
                let (inner, region_changed) = rewrite_body(&r.body);
                changed |= region_changed;
                Region::with_args(r.num_args, inner)
            })
            .collect();
        map.insert(
            id,
            new.push_with_regions(op.kind.clone(), operands, regions),
        );
    }

    (new, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{print_body, verify_body, Attr, Body, OpKind};

    fn run(body: &mut Body) -> Changed {
        let changed = SimplifyPass.run(body, &PassContext::default());
        verify_body(body).unwrap();
        changed
    }

    #[test]
    fn multiplicative_and_additive_identities_forward() {
        // `((a * 1) + 0) * 3` -- the inner identities forward straight
        // to `a`; the outer mul (result op, non-pow2) stays.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        let mul = body.push(OpKind::Mul, [a, one]);
        let zero = body.push(OpKind::Const(Attr::Int(0)), []);
        let add = body.push(OpKind::Add, [mul, zero]);
        let three = body.push(OpKind::Const(Attr::Int(3)), []);
        body.push(OpKind::Mul, [add, three]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.arg 0\n%1 = core.const 1\n%2 = core.const 0\n\
             %3 = core.const 3\n%4 = core.mul %0, %3\n"
        );
    }

    #[test]
    fn xor_self_is_zero_even_in_result_position() {
        // Value-producing rewrites are safe at the result op.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        body.push(OpKind::BitXor, [a, a]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(print_body(&body), "%0 = core.arg 0\n%1 = core.const 0\n");
    }

    #[test]
    fn eq_self_requires_int_proof() {
        // `a == a` with `a` of unknown type must NOT fold (could be a
        // float NaN at runtime)...
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        body.push(OpKind::Eq, [a, a]);
        let before = print_body(&body);
        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);

        // ...but a provably-int value folds fine.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let mask = body.push(OpKind::Const(Attr::Int(3)), []);
        let band = body.push(OpKind::BitAnd, [a, mask]);
        body.push(OpKind::Ne, [band, band]);
        assert_eq!(run(&mut body), Changed::Yes);
        assert!(print_body(&body).ends_with("core.const false\n"));
    }

    #[test]
    fn strength_reduction_only_under_int_proof() {
        // `(a & 7) * 4` -> `(a & 7) shl 2`.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let mask = body.push(OpKind::Const(Attr::Int(7)), []);
        let band = body.push(OpKind::BitAnd, [a, mask]);
        let four = body.push(OpKind::Const(Attr::Int(4)), []);
        body.push(OpKind::Mul, [band, four]);

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        assert!(printed.contains("core.shl"), "{printed}");
        assert!(!printed.contains("core.mul"), "{printed}");

        // `a * 4` with unknown `a`: no proof, no reduction.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let four = body.push(OpKind::Const(Attr::Int(4)), []);
        body.push(OpKind::Mul, [a, four]);
        assert_eq!(run(&mut body), Changed::No);
    }

    #[test]
    fn x_plus_x_becomes_shift_under_int_proof() {
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let mask = body.push(OpKind::Const(Attr::Int(7)), []);
        let band = body.push(OpKind::BitAnd, [a, mask]);
        let sum = body.push(OpKind::Add, [band, band]);
        body.push(OpKind::Tuple, [sum]);

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        assert!(printed.contains("core.shl %2, %3"), "{printed}");
    }

    #[test]
    fn double_negation_forwards() {
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let n1 = body.push(OpKind::Neg, [a]);
        let n2 = body.push(OpKind::Neg, [n1]);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        body.push(OpKind::Add, [n2, one]);

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        // `--a` forwarded to `a`; the first neg is now dead (DCE's job).
        assert!(printed.contains("core.add %0, %2"), "{printed}");
    }

    #[test]
    fn forwarding_is_suppressed_at_the_result_op() {
        // `a * 1` as the whole body: rewriting would leave `const 1` as
        // the (positional) result -- must stay put.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        body.push(OpKind::Mul, [a, one]);
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }
}
