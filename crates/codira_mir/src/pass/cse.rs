//! Copyright (c) 2026 Omnira CJSC
//!
//! Common subexpression elimination by hash-consing -- the classic
//! value-numbering sweep (KGEN analog: MLIR's `cse` pass as composed into
//! the KGEN pipeline; the e-graph crate `codira_egraph` subsumes this for
//! equality *modulo rewrites*, but a cheap syntactic CSE still pays for
//! itself as a pipeline cleanup).
//!
//! # Scope and policy
//!
//! * **Pure ops only** are hash-consed, keyed on `(kind, renumbered operands)`:
//!   `core.const`, all arithmetic/comparison/logic/bitwise/ shift ops,
//!   `neg`/`not`, `tuple`/`tuple_get`, and the reference ops (`core.arg`,
//!   `cf.block_arg`, `param.ref`). Because the sweep is in-order and operands
//!   are already renumbered, two ops with equal keys are structurally identical
//!   DAGs -- the later def forwards to the first occurrence, and the orphaned
//!   def is dropped from the rebuild (DCE-style; running `dce` afterwards is
//!   still worthwhile for the *operands* the dropped def was the last user of).
//!   `core.div`/`core.rem` are included: they can trap, but two identical divs
//!   trap identically, and deduplication keeps one.
//! * **`core.call`, `cf.if`, `cf.while`, `cf.for` keep their identity**: calls
//!   may have effects (merging two calls would drop one execution), and region
//!   ops are cheaper to leave to the e-graph than to compare structurally here.
//!   Their *regions* are still recursed into, so CSE happens inside loop and
//!   branch bodies.
//! * **No cross-region merging.** The value-number table is per region body. An
//!   op inside a region cannot reference a parent op by operand id anyway
//!   (region-local ids), so "merging" across the boundary is not even
//!   representable -- the table reset makes the non-goal structural.
//! * The body's **result op is never deduplicated away**: the result is
//!   positional (last op), so dropping the last op would silently reassign the
//!   result. Its operands are still renumbered.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{rewrite::remap_operands, Changed, Pass, PassContext};
use crate::op::{Body, OpId, OpKind, Region};

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct CsePass;

impl Pass for CsePass {
    fn name(&self) -> &'static str {
        "cse"
    }

    fn run(&self, body: &mut Body, _ctx: &PassContext<'_>) -> Changed {
        let (new, changed) = rewrite_body(body);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// Pure, region-free, hash-consable op kinds (module-doc list).
fn hashconsable(kind: &OpKind) -> bool {
    use OpKind::{
        Add, And, Arg, BitAnd, BitOr, BitXor, BlockArg, Const, Div, Eq, Ge, Gt, Le, Lt, Mul, Ne,
        Neg, Not, Or, ParamRef, Rem, Shl, Shr, Sub, Tuple, TupleGet,
    };
    matches!(
        kind,
        Const(_)
            | Add
            | Sub
            | Mul
            | Div
            | Rem
            | Neg
            | Eq
            | Ne
            | Lt
            | Le
            | Gt
            | Ge
            | And
            | Or
            | Not
            | BitAnd
            | BitOr
            | BitXor
            | Shl
            | Shr
            | Tuple
            | TupleGet(_)
            | Arg(_)
            | BlockArg(_)
            | ParamRef(_)
    )
}

fn rewrite_body(src: &Body) -> (Body, bool) {
    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut table: FxHashMap<(OpKind, SmallVec<[OpId; 2]>), OpId> = FxHashMap::default();
    let mut changed = false;
    let last = src.result();

    for (id, op) in src.iter() {
        let operands = remap_operands(op, &map);

        if hashconsable(&op.kind) && op.regions.is_empty() {
            let key = (op.kind.clone(), operands.clone());
            if let Some(&first) = table.get(&key) {
                if Some(id) != last {
                    map.insert(id, first);
                    changed = true;
                    continue;
                }
            }
            let new_id = new.push(op.kind.clone(), operands);
            table.entry(key).or_insert(new_id);
            map.insert(id, new_id);
            continue;
        }

        // Identity-preserving ops: copy, recursing into regions with a
        // fresh table each (no cross-region merging).
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
    use crate::{print_body, verify_body, Attr, Body, OpKind, Region};

    fn run(body: &mut Body) -> Changed {
        let changed = CsePass.run(body, &PassContext::default());
        verify_body(body).unwrap();
        changed
    }

    #[test]
    fn dedups_structurally_identical_dags() {
        // `(a + 1) * (a + 1)` built with duplicated const and add.
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let one_a = body.push(OpKind::Const(Attr::Int(1)), []);
        let add_a = body.push(OpKind::Add, [a, one_a]);
        let one_b = body.push(OpKind::Const(Attr::Int(1)), []);
        let add_b = body.push(OpKind::Add, [a, one_b]);
        body.push(OpKind::Mul, [add_a, add_b]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "%0 = core.arg 0\n%1 = core.const 1\n%2 = core.add %0, %1\n%3 = core.mul %2, %2\n"
        );
    }

    #[test]
    fn calls_are_never_merged() {
        let mut body = Body::new();
        let c = body.push(OpKind::Const(Attr::Int(1)), []);
        let call_a = body.push(OpKind::Call("f".into()), [c]);
        let call_b = body.push(OpKind::Call("f".into()), [c]);
        body.push(OpKind::Add, [call_a, call_b]);
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }

    #[test]
    fn recurses_into_regions_but_not_across_them() {
        // Duplicate consts inside the then-region dedup; the identical
        // const at the top level is *not* merged with them.
        let mut body = Body::new();
        body.push(OpKind::Const(Attr::Int(7)), []);
        let cond = body.push(OpKind::Arg(0), []);
        let mut then_b = Body::new();
        let x = then_b.push(OpKind::Const(Attr::Int(7)), []);
        let y = then_b.push(OpKind::Const(Attr::Int(7)), []);
        then_b.push(OpKind::Add, [x, y]);
        let mut else_b = Body::new();
        else_b.push(OpKind::Const(Attr::Int(0)), []);
        body.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        // Top level still has its own const 7; then-region now has one.
        assert_eq!(printed.matches("core.const 7").count(), 2, "{printed}");
        assert!(printed.contains("core.add %0, %0"), "{printed}");
    }

    #[test]
    fn result_op_is_not_deduplicated() {
        // `[const 1, const 1]` -- the duplicate *is* the result; dropping
        // it would reassign the result, so it must be kept.
        let mut body = Body::new();
        body.push(OpKind::Const(Attr::Int(1)), []);
        body.push(OpKind::Const(Attr::Int(1)), []);
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }
}
