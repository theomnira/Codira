//! Copyright (c) 2026 Omnira CJSC
//!
//! Dead code elimination -- the cleanup pass every other pass leans on
//! (`const-fold`, `cse`, and `simplify` all *forward* values and leave
//! the orphaned defs behind on purpose; this pass sweeps them). KGEN
//! analog: the DCE/canonicalize cleanup interleaved through the pipeline
//! in `modular/KGEN/lib/Compiler/Pipeline/Pipeline.cpp`.
//!
//! # Liveness model
//!
//! Each region body is its own liveness domain (operand ids are
//! region-local). Within one body, the roots are:
//!
//! * the body's **result op** (its last op -- the positional result
//!   convention). For loop bodies that is the `cf.yield`, whose operands are
//!   the next iteration's carried values: rooting the yield is what keeps
//!   carried-value computations alive across the rebuild.
//! * every op that is, or (transitively, through any nesting of regions)
//!   **contains, a `core.call`**. Calls are conservatively treated as effectful
//!   -- the IR has no effect/purity annotations on callees, so an unused call
//!   may still write, print, or trap, and eliminating it would change
//!   observable behavior. This over-approximates (a dead `cf.if` wrapping a
//!   call is kept even if the call itself is pure) but is the only sound
//!   default. Loops *without* calls are assumed terminating and are eligible
//!   for deletion -- this IR's comptime semantics treats non-termination as an
//!   interpreter budget error, not an observable effect.
//!
//! Everything reachable from a root through operand edges is live; the
//! body is rebuilt keeping only live ops (in their original relative
//! order -- SSA order is preserved, so no re-sorting is needed),
//! renumbering operands through an old->new map, and recursing into the
//! regions of surviving ops with the same rules. `Region::num_args` is
//! preserved verbatim ([`Region::with_args`]), so `cf.block_arg`
//! references inside surviving regions stay valid without adjustment.

use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use super::{rewrite::remap_operands, Changed, Pass, PassContext};
use crate::op::{Body, Op, OpId, OpKind, Region};

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct DcePass;

impl Pass for DcePass {
    fn name(&self) -> &'static str {
        "dce"
    }

    fn run(&self, body: &mut Body, _ctx: &PassContext<'_>) -> Changed {
        let (new, changed) = rewrite_body(body);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// True when `op` is a call or any of its regions (recursively) contains
/// one -- the conservative-liveness predicate from the module doc.
fn contains_call(op: &Op) -> bool {
    matches!(op.kind, OpKind::Call(_))
        || op
            .regions
            .iter()
            .any(|r| r.body.iter().any(|(_, nested)| contains_call(nested)))
}

fn rewrite_body(src: &Body) -> (Body, bool) {
    // Mark: roots are the result op plus every call-containing op.
    let mut live: FxHashSet<OpId> = FxHashSet::default();
    let mut worklist: Vec<OpId> = Vec::new();
    if let Some(result) = src.result() {
        worklist.push(result);
    }
    for (id, op) in src.iter() {
        if contains_call(op) {
            worklist.push(id);
        }
    }
    while let Some(id) = worklist.pop() {
        if live.insert(id) {
            worklist.extend(src.get(id).operands.iter().copied());
        }
    }

    // Sweep: rebuild in order, keeping live ops and recursing into their
    // regions (each region is swept against its own result/yield root).
    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut changed = false;
    for (id, op) in src.iter() {
        if !live.contains(&id) {
            changed = true;
            continue;
        }
        let operands = remap_operands(op, &map);
        let regions: SmallVec<[Region; 0]> = op
            .regions
            .iter()
            .map(|r| {
                let (swept, region_changed) = rewrite_body(&r.body);
                changed |= region_changed;
                Region::with_args(r.num_args, swept)
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
        let changed = DcePass.run(body, &PassContext::default());
        verify_body(body).unwrap();
        changed
    }

    #[test]
    fn drops_unused_pure_ops_and_renumbers() {
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        body.push(OpKind::Const(Attr::Int(5)), []); // dead
        body.push(OpKind::Add, [a, a]);

        assert_eq!(run(&mut body), Changed::Yes);
        assert_eq!(print_body(&body), "%0 = core.arg 0\n%1 = core.add %0, %0\n");
    }

    #[test]
    fn unused_call_is_conservatively_kept() {
        let mut body = Body::new();
        let c = body.push(OpKind::Const(Attr::Int(1)), []);
        body.push(OpKind::Call("effectful".into()), [c]);
        body.push(OpKind::Const(Attr::Int(2)), []); // the (unrelated) result
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }

    #[test]
    fn dead_if_containing_call_is_kept() {
        let mut body = Body::new();
        let cond = body.push(OpKind::Arg(0), []);
        let mut then_b = Body::new();
        then_b.push(OpKind::Call("effectful".into()), []);
        let mut else_b = Body::new();
        else_b.push(OpKind::Const(Attr::Unit), []);
        body.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );
        body.push(OpKind::Const(Attr::Int(0)), []); // result; the if is unused
        let before = print_body(&body);

        assert_eq!(run(&mut body), Changed::No);
        assert_eq!(print_body(&body), before);
    }

    #[test]
    fn loop_body_yield_roots_carried_computation() {
        // `for iv in 0..3 iter(acc) { dead; acc + iv }` -- the add feeds
        // the yield and must survive; the unused const inside must not.
        let mut body = Body::new();
        let start = body.push(OpKind::Const(Attr::Int(0)), []);
        let end = body.push(OpKind::Const(Attr::Int(3)), []);
        let step = body.push(OpKind::Const(Attr::Int(1)), []);
        let init = body.push(OpKind::Const(Attr::Int(0)), []);

        let mut loop_b = Body::new();
        let iv = loop_b.push(OpKind::BlockArg(0), []);
        let acc = loop_b.push(OpKind::BlockArg(1), []);
        loop_b.push(OpKind::Const(Attr::Int(99)), []); // dead inside the loop
        let add = loop_b.push(OpKind::Add, [acc, iv]);
        loop_b.push(OpKind::Yield, [add]);

        body.push_with_regions(
            OpKind::For,
            [start, end, step, init],
            [Region::with_args(2, loop_b)],
        );

        assert_eq!(run(&mut body), Changed::Yes);
        let printed = print_body(&body);
        assert!(
            !printed.contains("99"),
            "dead in-loop const removed:\n{printed}"
        );
        assert!(
            printed.contains("core.add"),
            "carried computation kept:\n{printed}"
        );
        assert!(
            printed.contains("body(2)"),
            "region num_args preserved:\n{printed}"
        );
    }
}
