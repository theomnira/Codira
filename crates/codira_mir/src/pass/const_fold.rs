//! Copyright (c) 2026 Omnira CJSC
//!
//! Forward constant propagation and folding -- the pass-pipeline face of
//! [`crate::fold::fold_op`], which remains the *only* place op arithmetic
//! is defined (KGEN reuses MLIR fold hooks for exactly this reason:
//! comptime interpretation, canonicalization, and constant folding must
//! share one semantics -- `modular/KGEN/docs/overviews/Interpreter.md`;
//! the pass analog is KGEN's canonicalize/SCCP layer under
//! `modular/KGEN/lib/Transforms/`).
//!
//! One forward pass per body (operands only point backwards, so a single
//! in-order sweep propagates constants transitively), rebuilding the body
//! and recursing into every region. Three rewrites:
//!
//! 1. **Pure-op folding**: an op whose renumbered operands are all `core.const`
//!    is replaced by `core.const(fold_op(..))` -- but only when `fold_op`
//!    succeeds. A genuine evaluation error (`DivideByZero`, `DivisionOverflow`,
//!    `ShiftOutOfRange`, a type mismatch) means the op is **left alone**: it
//!    must keep failing at interpretation/run time, not silently vanish or
//!    become a value. `NotFoldable` (control flow, calls, references) is also
//!    "leave it".
//!
//! 2. **`cf.if` with a constant condition**: the taken region is folded
//!    recursively, then spliced inline in place of the `If`; the untaken region
//!    is dropped (its side effects are unreachable by definition). An empty
//!    taken region (elided `else`) becomes `core.const unit`, matching `If`'s
//!    documented result convention.
//!
//!    *Why splicing is `BlockArg`-index-stable*: `cf.if` regions have
//!    `num_args == 0` -- they are **transparent** frames. A
//!    `cf.block_arg i` inside one refers to the innermost enclosing
//!    region that *carries* arguments (a loop body), and splicing the
//!    `If` region's ops into the parent body does not change which
//!    region that is: the parent is either that very loop body or
//!    another transparent region under it. So the copied `BlockArg` ops
//!    keep their indices verbatim and remain correct -- no substitution
//!    map needed ([`rewrite::splice`] with `Substitution::none()`).
//!
//! 3. **`core.tuple_get(i)` of a `core.tuple`**: forwards to the tuple's i-th
//!    operand. Suppressed when the `tuple_get` is the body's result op -- a
//!    body's result is *positional* (last op), so forwarding there would
//!    silently change the result to an unrelated op. The suppressed form is
//!    still correct IR, and it is exactly the shape [`rewrite::ensure_result`]
//!    emits, so the two agree instead of ping-ponging.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{
    rewrite::{remap_operands, splice, Substitution},
    Changed, Pass, PassContext,
};
use crate::{
    fold::fold_op,
    op::{Body, OpId, OpKind, Region},
    Attr,
};

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct ConstFoldPass;

impl Pass for ConstFoldPass {
    fn name(&self) -> &'static str {
        "const-fold"
    }

    fn run(&self, body: &mut Body, _ctx: &PassContext<'_>) -> Changed {
        let (new, changed) = rewrite_body(body);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// Rebuilds `src` with constants propagated; returns the new body and
/// whether anything changed. Recurses into all regions.
fn rewrite_body(src: &Body) -> (Body, bool) {
    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut changed = false;
    let last = src.result();

    for (id, op) in src.iter() {
        // (2) `cf.if` on a constant condition: splice the taken region.
        if matches!(op.kind, OpKind::If) {
            let cond = map[&op.operands[0]];
            if let OpKind::Const(Attr::Bool(b)) = &new.get(cond).kind {
                let taken = &op.regions[usize::from(!*b)];
                let (folded, _) = rewrite_body(&taken.body);
                let spliced = splice(&mut new, &folded, &Substitution::none());
                let value = spliced
                    .result
                    .unwrap_or_else(|| new.push(OpKind::Const(Attr::Unit), []));
                map.insert(id, value);
                changed = true;
                continue;
            }
        }

        // (3) `tuple_get(i)` of a `tuple`: forward the element. Guarded
        // against result position (see module doc).
        if let OpKind::TupleGet(i) = op.kind {
            let tuple = map[&op.operands[0]];
            if matches!(new.get(tuple).kind, OpKind::Tuple) {
                if let Some(&element) = new.get(tuple).operands.get(i as usize) {
                    if Some(id) != last {
                        map.insert(id, element);
                        changed = true;
                        continue;
                    }
                }
            }
        }

        let operands = remap_operands(op, &map);

        // (0) A cast with a constant operand. Handled before the generic
        // all-constant fold below because `fold_op` cannot express a cast:
        // the result depends on the *target* type, which lives on the op
        // rather than in the `OpKind`. See `fold::fold_cast`.
        if let OpKind::Cast(kind, crate::op::CastMode::Wrapping) = &op.kind {
            if let Some(&operand) = operands.first() {
                let source_ty = new.get(operand).ty;
                if let OpKind::Const(attr) = &new.get(operand).kind {
                    if let Ok(folded) = crate::fold::fold_cast(*kind, attr, source_ty, op.ty) {
                        map.insert(id, new.push_typed(OpKind::Const(folded), [], op.ty));
                        changed = true;
                        continue;
                    }
                }
            }
        }

        // (1) All-constant pure op: fold through `fold_op`. `Const` ops
        // themselves are skipped (re-folding a const to itself would
        // report a spurious change forever).
        if !matches!(op.kind, OpKind::Const(_)) && op.regions.is_empty() {
            let attrs: Option<Vec<Attr>> = operands
                .iter()
                .map(|&o| match &new.get(o).kind {
                    OpKind::Const(attr) => Some(attr.clone()),
                    _ => None,
                })
                .collect();
            if let Some(attrs) = attrs {
                if let Ok(folded) = fold_op(&op.kind, &attrs) {
                    map.insert(id, new.push(OpKind::Const(folded), []));
                    changed = true;
                    continue;
                }
                // Err: evaluation error (keep the failing op for the
                // interpreter to report) or NotFoldable -- leave alone.
            }
        }

        // Default: copy the op, recursing into its regions.
        let regions: SmallVec<[Region; 0]> = op
            .regions
            .iter()
            .map(|r| {
                let (folded, region_changed) = rewrite_body(&r.body);
                changed |= region_changed;
                Region::with_args(r.num_args, folded)
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
    use crate::{print_body, verify_body, Attr, Body, OpKind, Region};

    fn run(body: &mut Body) -> Changed {
        let changed = ConstFoldPass.run(body, &PassContext::default());
        verify_body(body).unwrap();
        changed
    }

    #[test]
    fn folds_constant_chains_in_one_sweep() {
        // `(2 * 3) + 4` -- the add sees the folded mul in the same pass.
        let mut body = Body::new();
        let two = body.push(OpKind::Const(Attr::Int(2)), []);
        let three = body.push(OpKind::Const(Attr::Int(3)), []);
        let mul = body.push(OpKind::Mul, [two, three]);
        let four = body.push(OpKind::Const(Attr::Int(4)), []);
        body.push(OpKind::Add, [mul, four]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.const 2\n%1 = core.const 3\n%2 = core.const 6\n\
             %3 = core.const 4\n%4 = core.const 10\n"
        );
    }

    #[test]
    fn division_by_zero_is_left_alone() {
        // `1 / 0` must keep failing at runtime, not disappear or fold.
        let mut body = Body::new();
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        let zero = body.push(OpKind::Const(Attr::Int(0)), []);
        body.push(OpKind::Div, [one, zero]);
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }

    #[test]
    fn folds_if_with_constant_condition_by_splicing() {
        // `if true { 1 + 2 } else { 0 }` -> spliced then-region, folded.
        let mut body = Body::new();
        let cond = body.push(OpKind::Const(Attr::Bool(true)), []);
        let mut then_b = Body::new();
        let a = then_b.push(OpKind::Const(Attr::Int(1)), []);
        let b = then_b.push(OpKind::Const(Attr::Int(2)), []);
        then_b.push(OpKind::Add, [a, b]);
        let mut else_b = Body::new();
        else_b.push(OpKind::Const(Attr::Int(0)), []);
        body.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.const true\n%1 = core.const 1\n%2 = core.const 2\n%3 = core.const 3\n"
        );
    }

    #[test]
    fn empty_taken_region_becomes_unit() {
        // `if false { 1 } else { }` -> `const unit`.
        let mut body = Body::new();
        let cond = body.push(OpKind::Const(Attr::Bool(false)), []);
        let mut then_b = Body::new();
        then_b.push(OpKind::Const(Attr::Int(1)), []);
        body.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(Body::new())],
        );

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.const false\n%1 = core.const unit\n"
        );
    }

    #[test]
    fn if_splice_inside_loop_body_keeps_block_args_stable() {
        // A `cf.for` whose body contains `if true { block_arg 1 + 1 }`.
        // Splicing the transparent then-region into the loop body must
        // leave `cf.block_arg 1` pointing at the same carried value --
        // the If frame was transparent, the loop frame is unchanged.
        let mut body = Body::new();
        let start = body.push(OpKind::Const(Attr::Int(0)), []);
        let end = body.push(OpKind::Const(Attr::Int(3)), []);
        let step = body.push(OpKind::Const(Attr::Int(1)), []);
        let init = body.push(OpKind::Const(Attr::Int(0)), []);

        let mut loop_b = Body::new();
        let cond = loop_b.push(OpKind::Const(Attr::Bool(true)), []);
        let mut then_b = Body::new();
        let acc = then_b.push(OpKind::BlockArg(1), []);
        let one = then_b.push(OpKind::Const(Attr::Int(1)), []);
        then_b.push(OpKind::Add, [acc, one]);
        let mut else_b = Body::new();
        else_b.push(OpKind::BlockArg(1), []);
        let if_op = loop_b.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );
        loop_b.push(OpKind::Yield, [if_op]);

        body.push_with_regions(
            OpKind::For,
            [start, end, step, init],
            [Region::with_args(2, loop_b)],
        );

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        assert!(
            !printed.contains("cf.if"),
            "if was spliced away:\n{printed}"
        );
        assert!(
            printed.contains("cf.block_arg 1"),
            "carried-value reference survived with a stable index:\n{printed}"
        );
        assert!(!printed.contains("%?"), "no dangling operands:\n{printed}");
    }

    #[test]
    fn tuple_get_of_tuple_forwards_and_enables_folding() {
        // `t = (a, 1); t[1] + t[1]` -> the projections forward to the
        // const element, and the add folds to 2.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        let tuple = body.push(OpKind::Tuple, [a, one]);
        let get = body.push(OpKind::TupleGet(1), [tuple]);
        body.push(OpKind::Add, [get, get]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.arg 0\n%1 = core.const 1\n%2 = core.tuple %0, %1\n%3 = core.const 2\n"
        );
    }

    #[test]
    fn tuple_get_in_result_position_is_not_forwarded() {
        // Forwarding the last op would reassign the body's (positional)
        // result to an unrelated op -- must be suppressed.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        let tuple = body.push(OpKind::Tuple, [a, one]);
        body.push(OpKind::TupleGet(0), [tuple]);
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }
}
