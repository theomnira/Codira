//! Copyright (c) 2026 Omnira CJSC
//!
//! Full loop unrolling for `cf.for` with compile-time-constant bounds --
//! the pass that turns comptime-counted loops into straight-line code the
//! rest of the pipeline (const-fold, cse, dce) can chew through. KGEN
//! analog: the loop transformations in `modular/KGEN/lib/Transforms/`
//! feeding `LowerLoops`; unrolling *before* lowering is exactly why KGEN
//! keeps structured `hlcf.for` around so long -- trip counts are legible
//! there and gone after CFG flattening.
//!
//! # Eligibility
//!
//! A `cf.for` is unrolled when:
//! * `start`/`end`/`step` are all `core.const int` (after this pass's own
//!   bottom-up rewriting -- inner loops are processed first, so an outer unroll
//!   that substitutes a constant induction variable can make an inner loop
//!   eligible on the *next* pipeline iteration),
//! * the computed trip count is `0..=32` (0 forwards the inits; the cap keeps
//!   code growth bounded, same spirit as LLVM's default full-unroll threshold),
//!   computed with overflow-checked stepping so `step == 0` or wrap-around
//!   degenerates to "not unrollable" rather than a hang,
//! * the *scoping precheck* below passes.
//!
//! # The nested-`BlockArg` scoping rule (the subtle part)
//!
//! Splicing the body region into the parent substitutes `cf.block_arg 0`
//! (the induction variable) with the per-iteration constant and
//! `cf.block_arg 1..` with the current carried values. Substitution
//! applies at the **top level of the body region only**, plus nested
//! *transparent* regions (`cf.if` branches, `num_args == 0`) -- a nested
//! region with `num_args > 0` (an inner loop) opens its own frame, and
//! `BlockArg`s inside it refer to *that* loop, so [`rewrite::splice`]
//! leaves them strictly alone.
//!
//! Inside transparent nested regions there is an extra representational
//! constraint (see `rewrite.rs`'s module doc): operand ids cannot cross a
//! region boundary, so a substituted reference there must be
//! *re-materializable*. The induction variable always is (it is a fresh
//! `core.const` each iteration), but a carried value is whatever the
//! previous iteration's yield produced -- an arbitrary op. Rather than
//! reason per-iteration (and risk aborting halfway with side-effectful
//! ops already spliced), the precheck is conservative and iteration-
//! independent: **if any transparent nested region references
//! `cf.block_arg i` with `i > 0`, the loop is not unrolled.** Missing
//! that (rare) shape costs an optimization, never correctness.
//!
//! # Result convention
//!
//! The final carried values replace the `cf.for`'s result following the
//! loop-result convention documented on [`OpKind::While`]: the value
//! itself for N == 1, a `core.tuple` for N > 1, `core.const unit` for
//! N == 0. Trip count 0 uses the init values directly. Because the
//! replacement may be an *earlier* op (e.g. trip count 0), the rebuild
//! finishes with [`rewrite::ensure_result`] to keep the body's positional
//! result honest.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{
    rewrite::{const_int, ensure_result, remap_operands, splice, Substitution},
    Changed, Pass, PassContext,
};
use crate::{
    op::{Body, OpId, OpKind, Region},
    Attr,
};

/// Full unrolling beyond this trip count is assumed unprofitable.
const MAX_TRIP_COUNT: usize = 32;

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnrollPass;

impl Pass for UnrollPass {
    fn name(&self) -> &'static str {
        "loop-unroll"
    }

    fn run(&self, body: &mut Body, _ctx: &PassContext<'_>) -> Changed {
        let (new, changed) = rewrite_body(body);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// The induction-variable values of an unrollable loop, in order.
/// `None`: bounds not constant, trip count over the cap, or degenerate
/// stepping (`step == 0`, overflow).
fn trip_ivs(start: i64, end: i64, step: i64) -> Option<Vec<i64>> {
    let mut ivs = Vec::new();
    let mut iv = start;
    loop {
        // `cf.for` semantics per op.rs: iterate while
        // `step > 0 ? iv < end : iv > end`.
        let continues = if step > 0 { iv < end } else { iv > end };
        if !continues {
            return Some(ivs);
        }
        if ivs.len() == MAX_TRIP_COUNT {
            return None; // over the cap (or step == 0: never terminates)
        }
        ivs.push(iv);
        iv = iv.checked_add(step)?;
    }
}

/// The module doc's scoping precheck: no `cf.block_arg i` with `i > 0`
/// inside any transparent nested region of the loop body. Regions with
/// their own arguments are skipped wholesale -- their `BlockArg`s (and
/// those of transparent regions *under* them) belong to the inner frame.
fn carried_refs_stay_top_level(body: &Body) -> bool {
    fn scan(body: &Body, in_nested_transparent: bool) -> bool {
        for (_, op) in body.iter() {
            if in_nested_transparent {
                if let OpKind::BlockArg(i) = op.kind {
                    if i > 0 {
                        return false;
                    }
                }
            }
            for region in &op.regions {
                if region.num_args == 0 && !scan(&region.body, true) {
                    return false;
                }
            }
        }
        true
    }
    scan(body, false)
}

fn rewrite_body(src: &Body) -> (Body, bool) {
    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut changed = false;
    let last = src.result();

    for (id, op) in src.iter() {
        if matches!(op.kind, OpKind::For) {
            let operands = remap_operands(op, &map);
            // Bottom-up: rewrite (and possibly unroll) inner loops first.
            let (loop_body, region_changed) = rewrite_body(&op.regions[0].body);
            changed |= region_changed;

            let bounds = (
                const_int(&new, operands[0]),
                const_int(&new, operands[1]),
                const_int(&new, operands[2]),
            );
            let plan = match bounds {
                (Some(start), Some(end), Some(step)) => {
                    trip_ivs(start, end, step).filter(|_| carried_refs_stay_top_level(&loop_body))
                }
                _ => None,
            };

            let Some(ivs) = plan else {
                // Not unrollable: copy the loop with its rewritten body.
                let num_args = op.regions[0].num_args;
                map.insert(
                    id,
                    new.push_with_regions(
                        op.kind.clone(),
                        operands,
                        [Region::with_args(num_args, loop_body)],
                    ),
                );
                continue;
            };

            // Splice one copy per iteration, threading carried values.
            let mut carried: Vec<OpId> = operands[3..].to_vec();
            for iv in ivs {
                let iv_id = new.push(OpKind::Const(Attr::Int(iv)), []);
                let mut block_args = Vec::with_capacity(1 + carried.len());
                block_args.push(iv_id);
                block_args.extend(carried.iter().copied());
                let spliced = splice(
                    &mut new,
                    &loop_body,
                    &Substitution {
                        block_args: Some(&block_args),
                        args: None,
                    },
                );
                carried = spliced
                    .yielded
                    .expect("verified cf.for body ends in cf.yield");
            }

            // Final carried values become the For's result value, per the
            // While/For result convention.
            let value = match carried.len() {
                0 => new.push(OpKind::Const(Attr::Unit), []),
                1 => carried[0],
                _ => new.push(OpKind::Tuple, carried.iter().copied()),
            };
            map.insert(id, value);
            changed = true;
            continue;
        }

        // Any other op: copy, recursing into regions (unroll inside
        // while-bodies and if-branches too).
        let operands = remap_operands(op, &map);
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

    // Keep the positional result honest if an unrolled For at the tail
    // was replaced by an earlier value (trip 0 / pass-through yields).
    if let Some(src_result) = last {
        let mapped = map[&src_result];
        if new.result() != Some(mapped) {
            ensure_result(&mut new, mapped);
        }
    }

    (new, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        pass::{ConstFoldPass, CsePass, DcePass, PassManager},
        print_body, verify_body, Attr, Body, OpKind, Region,
    };

    /// `for iv in start..end step 1 iter(acc = 0) { acc + iv }`.
    fn accumulating_for(body: &mut Body, start: i64, end: i64) -> OpId {
        let start = body.push(OpKind::Const(Attr::Int(start)), []);
        let end = body.push(OpKind::Const(Attr::Int(end)), []);
        let step = body.push(OpKind::Const(Attr::Int(1)), []);
        let init = body.push(OpKind::Const(Attr::Int(0)), []);
        let mut loop_b = Body::new();
        let iv = loop_b.push(OpKind::BlockArg(0), []);
        let acc = loop_b.push(OpKind::BlockArg(1), []);
        let add = loop_b.push(OpKind::Add, [acc, iv]);
        loop_b.push(OpKind::Yield, [add]);
        body.push_with_regions(
            OpKind::For,
            [start, end, step, init],
            [Region::with_args(2, loop_b)],
        )
    }

    fn cleanup() -> PassManager {
        PassManager::new()
            .add(UnrollPass)
            .add(ConstFoldPass)
            .add(CsePass)
            .add(DcePass)
    }

    #[test]
    fn unrolls_accumulation_to_a_single_constant() {
        // `for iv in 0..4 { acc = acc + iv }` == 0+1+2+3 == 6.
        let mut body = Body::new();
        accumulating_for(&mut body, 0, 4);

        cleanup()
            .run_to_fixpoint(&mut body, &PassContext::default(), 8)
            .unwrap();
        assert_eq!(print_body(&body), "%0 = core.const 6\n");
    }

    #[test]
    fn trip_count_zero_forwards_the_inits() {
        // `1 + (for iv in 5..5 iter(acc = 0) { .. })` -- the loop body
        // never runs; its value is the init.
        let mut body = Body::new();
        let for_op = accumulating_for(&mut body, 5, 5);
        let one = body.push(OpKind::Const(Attr::Int(1)), []);
        body.push(OpKind::Add, [for_op, one]);

        cleanup()
            .run_to_fixpoint(&mut body, &PassContext::default(), 8)
            .unwrap();
        assert_eq!(print_body(&body), "%0 = core.const 1\n");
    }

    #[test]
    fn over_cap_and_zero_step_loops_are_left_alone() {
        let mut body = Body::new();
        accumulating_for(&mut body, 0, 100);
        let before = print_body(&body);
        assert_eq!(
            UnrollPass.run(&mut body, &PassContext::default()),
            Changed::No
        );
        assert_eq!(print_body(&body), before);

        // step == 0 with start > end would spin forever; must be skipped.
        let mut body = Body::new();
        let start = body.push(OpKind::Const(Attr::Int(5)), []);
        let end = body.push(OpKind::Const(Attr::Int(0)), []);
        let step = body.push(OpKind::Const(Attr::Int(0)), []);
        let mut loop_b = Body::new();
        loop_b.push(OpKind::BlockArg(0), []);
        loop_b.push(OpKind::Yield, []);
        body.push_with_regions(
            OpKind::For,
            [start, end, step],
            [Region::with_args(1, loop_b)],
        );
        assert_eq!(
            UnrollPass.run(&mut body, &PassContext::default()),
            Changed::No
        );
        verify_body(&body).unwrap();
    }

    #[test]
    fn nested_loop_scoping_is_respected() {
        // outer: `for i in 1..3 iter(sum = 0) { inner = for j in 0..i
        // iter(acc = sum) { acc + j }; yield inner }`. The inner loop's
        // `end` is the *outer* induction variable, so the inner loop is
        // not unrollable until the outer one substitutes constants.
        let mut body = Body::new();
        let o_start = body.push(OpKind::Const(Attr::Int(1)), []);
        let o_end = body.push(OpKind::Const(Attr::Int(3)), []);
        let o_step = body.push(OpKind::Const(Attr::Int(1)), []);
        let o_init = body.push(OpKind::Const(Attr::Int(0)), []);

        let mut outer_b = Body::new();
        let i = outer_b.push(OpKind::BlockArg(0), []);
        let sum = outer_b.push(OpKind::BlockArg(1), []);
        let i_start = outer_b.push(OpKind::Const(Attr::Int(0)), []);
        let i_step = outer_b.push(OpKind::Const(Attr::Int(1)), []);
        let mut inner_b = Body::new();
        let j = inner_b.push(OpKind::BlockArg(0), []);
        let acc = inner_b.push(OpKind::BlockArg(1), []);
        let add = inner_b.push(OpKind::Add, [acc, j]);
        inner_b.push(OpKind::Yield, [add]);
        let inner = outer_b.push_with_regions(
            OpKind::For,
            [i_start, i, i_step, sum],
            [Region::with_args(2, inner_b)],
        );
        outer_b.push(OpKind::Yield, [inner]);

        body.push_with_regions(
            OpKind::For,
            [o_start, o_end, o_step, o_init],
            [Region::with_args(2, outer_b)],
        );

        // Phase 1: a single unroll run expands only the outer loop. The
        // two inner copies must keep their own frames: their bodies still
        // reference `cf.block_arg 0/1` (j, acc), untouched by the outer
        // substitution, while their `end`/`init` operands now point at
        // the substituted outer values.
        assert_eq!(
            UnrollPass.run(&mut body, &PassContext::default()),
            Changed::Yes
        );
        verify_body(&body).unwrap();
        let printed = print_body(&body);
        assert_eq!(printed.matches("cf.for(").count(), 2, "{printed}");
        assert_eq!(printed.matches("cf.block_arg 0").count(), 2, "{printed}");
        assert_eq!(printed.matches("cf.block_arg 1").count(), 2, "{printed}");
        assert!(!printed.contains("%?"), "no dangling operands:\n{printed}");

        // Phase 2: iterating the cleanup pipeline now unrolls the inner
        // loops (their bounds became constants) and folds everything:
        // i=1: acc=0 (+j=0) -> 0; i=2: 0+0+1 -> 1.
        cleanup()
            .run_to_fixpoint(&mut body, &PassContext::default(), 8)
            .unwrap();
        assert_eq!(print_body(&body), "%0 = core.const 1\n");
    }

    #[test]
    fn carried_ref_inside_if_region_blocks_unrolling() {
        // `for iv in 0..2 iter(acc=0) { if c { block_arg 1 } else { block_arg 1 } }`
        // -- the carried value is referenced inside a transparent nested
        // region; per the scoping precheck the loop must be left alone.
        let mut body = Body::new();
        let start = body.push(OpKind::Const(Attr::Int(0)), []);
        let end = body.push(OpKind::Const(Attr::Int(2)), []);
        let step = body.push(OpKind::Const(Attr::Int(1)), []);
        let init = body.push(OpKind::Const(Attr::Int(0)), []);
        let mut loop_b = Body::new();
        let cond = loop_b.push(OpKind::Arg(0), []);
        let mut then_b = Body::new();
        then_b.push(OpKind::BlockArg(1), []);
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
        let before = print_body(&body);

        assert_eq!(
            UnrollPass.run(&mut body, &PassContext::default()),
            Changed::No
        );
        assert_eq!(print_body(&body), before);
    }
}
