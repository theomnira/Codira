//! Copyright (c) 2026 Omnira CJSC
//!
//! `inline` -- replaces a `core.call` with the callee's body.
//!
//! KGEN analog: `modular/KGEN/lib/Transforms/AutomaticInline.cpp` (plus
//! `ApplyInliner` on the parametric side). The motivation is the same one
//! KGEN's pipeline documents: inlining is not primarily about call
//! overhead, it is about *exposing* the callee's operations to the
//! caller's constant propagation, CSE, and algebraic rewrites. A
//! `@double(4)` that stays a call is opaque; inlined, it collapses to a
//! constant.
//!
//! # Policy: depth-1, elaborated callees only
//!
//! A candidate callee must satisfy three conditions, each checked rather
//! than assumed:
//!
//! 1. **Resolvable.** [`PassContext::generators`] is present and
//!    [`GeneratorStore::generator_by_name`] finds the symbol. Without a store
//!    there is no interprocedural context and every call is left alone -- so
//!    running this pass on a bare body is a well-defined no-op, not a panic.
//! 2. **Fully elaborated.** No `param.ref` survives anywhere in the callee. A
//!    generator still carrying unresolved parameters has not been specialized
//!    yet; splicing it into a caller would silently capture the *caller's*
//!    (nonexistent) parameter bindings. Elaboration runs before this pass --
//!    see `codira_comptime::elaborate`.
//! 3. **Call-free.** The callee itself contains no `core.call`. This is a
//!    deliberate depth-1 policy rather than a recursion guard bolted on:
//!    running the pass to fixpoint (see
//!    [`PassManager::run_to_fixpoint`](super::PassManager::run_to_fixpoint))
//!    inlines a chain `a -> b -> c` one level per iteration, from the leaves
//!    up, and a *recursive* callee never becomes call-free, so it is never
//!    inlined and the fixpoint terminates. Depth-1 plus fixpoint gives chain
//!    inlining and recursion-safety from a single rule, instead of a separate
//!    depth counter that would have to be threaded through every rewrite.
//!
//! A call whose operand count is smaller than the callee's argument use
//! demands (`core.arg(i)` for some `i >= operands.len()`) is an
//! arity mismatch: left alone, since inlining it would fabricate values.
//! The IR verifier cannot catch this -- `core.call` is untyped and the
//! callee lives in a different body -- so this pass is where an
//! interprocedural arity error is first observable, and it degrades to
//! "no inlining" rather than to unsound IR.
//!
//! # Scoping
//!
//! Substitution is `Arg(i) -> operands[i]`, which is *function*-scoped:
//! unlike `cf.block_arg`, `core.arg` means the same thing at every nesting
//! depth inside the callee, so the substitution applies through nested
//! regions too (including the callee's own loop bodies, whose
//! `cf.block_arg`s belong to those loops and are copied verbatim). The
//! re-materialization rules for references reached inside a nested region
//! live in [`super::rewrite`]; this pass gates every splice on
//! [`can_splice`] and skips the call when a substitution could not be
//! expressed there.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::{
    rewrite::{can_splice, ensure_result, remap_operands, splice, Substitution},
    Changed, Pass, PassContext,
};
use crate::{
    op::{Body, OpId, OpKind, Region},
    Attr,
};

/// See the module doc.
#[derive(Debug, Default, Clone, Copy)]
pub struct InlinePass;

impl Pass for InlinePass {
    fn name(&self) -> &'static str {
        "inline"
    }

    fn run(&self, body: &mut Body, ctx: &PassContext<'_>) -> Changed {
        // No interprocedural context: nothing is resolvable, so this is a
        // defined no-op (condition 1 in the module doc).
        if ctx.generators.is_none() {
            return Changed::No;
        }
        let (new, changed) = rewrite_body(body, ctx);
        if changed {
            *body = new;
        }
        Changed::from_bool(changed)
    }
}

/// What disqualifies a callee body from being inlined, or `None` when it
/// is a valid candidate. Computed over the whole body including nested
/// regions.
struct CalleeShape {
    has_param_ref: bool,
    has_call: bool,
    /// One past the highest `core.arg` index referenced, i.e. the number
    /// of runtime arguments the body actually demands.
    arity: u32,
}

fn callee_shape(body: &Body) -> CalleeShape {
    let mut shape = CalleeShape {
        has_param_ref: false,
        has_call: false,
        arity: 0,
    };
    walk(body, &mut shape);
    shape
}

fn walk(body: &Body, shape: &mut CalleeShape) {
    for (_, op) in body.iter() {
        match &op.kind {
            OpKind::ParamRef(_) | OpKind::Const(Attr::ParamRef(_)) => shape.has_param_ref = true,
            OpKind::Call(_) => shape.has_call = true,
            OpKind::Arg(i) => shape.arity = shape.arity.max(*i + 1),
            _ => {}
        }
        for region in &op.regions {
            walk(&region.body, shape);
        }
    }
}

/// Rebuilds `src` with every inlinable `core.call` replaced by the
/// callee's body; returns the new body and whether anything changed.
/// Recurses into regions, so a call inside a loop body is inlined too.
fn rewrite_body(src: &Body, ctx: &PassContext<'_>) -> (Body, bool) {
    let store = ctx
        .generators
        .expect("caller checked for a generator store");

    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut changed = false;
    let last = src.result();

    for (id, op) in src.iter() {
        let operands = remap_operands(op, &map);

        if let OpKind::Call(symbol) = &op.kind {
            if let Some((_, callee)) = store.generator_by_name(symbol) {
                let shape = callee_shape(&callee.body);
                let inlinable = !shape.has_param_ref
                    && !shape.has_call
                    && shape.arity as usize <= operands.len()
                    && can_splice(
                        &new,
                        &callee.body,
                        &Substitution {
                            block_args: None,
                            args: Some(&operands),
                        },
                    );

                if inlinable {
                    let spliced = splice(
                        &mut new,
                        &callee.body,
                        &Substitution {
                            block_args: None,
                            args: Some(&operands),
                        },
                    );
                    // An empty callee computes nothing; `core.const unit`
                    // is its value, matching how `const-fold` materializes
                    // an empty taken `cf.if` branch.
                    let value = spliced
                        .result
                        .unwrap_or_else(|| new.push(OpKind::Const(Attr::Unit), []));
                    map.insert(id, value);
                    changed = true;
                    continue;
                }
            }
        }

        let regions: SmallVec<[Region; 0]> = op
            .regions
            .iter()
            .map(|r| {
                let (inlined, region_changed) = rewrite_body(&r.body, ctx);
                changed |= region_changed;
                Region::with_args(r.num_args, inlined)
            })
            .collect();
        map.insert(
            id,
            new.push_with_regions(op.kind.clone(), operands, regions),
        );
    }

    // Inlining a callee whose body is a bare `core.arg` maps the call to a
    // *pre-existing* caller op rather than appending anything, so when the
    // call sat in result position the body's positional result would
    // otherwise silently become whatever op happens to be last. See
    // `rewrite::ensure_result`.
    if changed {
        if let Some(result) = last.and_then(|l| map.get(&l).copied()) {
            ensure_result(&mut new, result);
        }
    }

    (new, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{print_body, verify_body, Generator, GeneratorStore, OpKind};

    /// `fn double(a) = a + a`
    fn double() -> Generator {
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let b = body.push(OpKind::Arg(0), []);
        body.push(OpKind::Add, [a, b]);
        Generator {
            name: "double".into(),
            params: vec![],
            body,
        }
    }

    fn run_with(body: &mut Body, store: &GeneratorStore) -> Changed {
        let ctx = PassContext {
            generators: Some(store),
        };
        let changed = InlinePass.run(body, &ctx);
        verify_body(body).unwrap();
        changed
    }

    #[test]
    fn inlines_a_simple_call() {
        let mut store = GeneratorStore::new();
        store.add_generator(double());

        // `double(7)`
        let mut body = Body::new();
        let seven = body.push(OpKind::Const(Attr::Int(7)), []);
        body.push(OpKind::Call("double".into()), [seven]);

        assert_eq!(run_with(&mut body, &store), Changed::Yes);
        assert_eq!(
            print_body(&body),
            "\
%0 = core.const 7
%1 = core.add %0, %0
"
        );
    }

    #[test]
    fn no_store_is_a_no_op() {
        let mut body = Body::new();
        let seven = body.push(OpKind::Const(Attr::Int(7)), []);
        body.push(OpKind::Call("double".into()), [seven]);
        let before = print_body(&body);

        assert_eq!(
            InlinePass.run(&mut body, &PassContext::default()),
            Changed::No
        );
        assert_eq!(print_body(&body), before);
    }

    #[test]
    fn unknown_symbol_is_left_alone() {
        let store = GeneratorStore::new();
        let mut body = Body::new();
        let seven = body.push(OpKind::Const(Attr::Int(7)), []);
        body.push(OpKind::Call("missing".into()), [seven]);

        assert_eq!(run_with(&mut body, &store), Changed::No);
    }

    #[test]
    fn unelaborated_callee_is_not_inlined() {
        // A callee still carrying `param.ref` has not been specialized;
        // splicing it would capture the wrong bindings (condition 2).
        let mut store = GeneratorStore::new();
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        let n = body.push(OpKind::ParamRef("N".into()), []);
        body.push(OpKind::Mul, [a, n]);
        store.add_generator(Generator {
            name: "scale".into(),
            params: vec![],
            body,
        });

        let mut caller = Body::new();
        let three = caller.push(OpKind::Const(Attr::Int(3)), []);
        caller.push(OpKind::Call("scale".into()), [three]);

        assert_eq!(run_with(&mut caller, &store), Changed::No);
    }

    #[test]
    fn recursive_callee_is_never_inlined() {
        // Self-recursion means never call-free, so condition 3 rejects it
        // forever -- which is exactly what makes the fixpoint terminate.
        let mut store = GeneratorStore::new();
        let mut body = Body::new();
        let a = body.push(OpKind::Arg(0), []);
        body.push(OpKind::Call("loopy".into()), [a]);
        store.add_generator(Generator {
            name: "loopy".into(),
            params: vec![],
            body,
        });

        let mut caller = Body::new();
        let one = caller.push(OpKind::Const(Attr::Int(1)), []);
        caller.push(OpKind::Call("loopy".into()), [one]);

        assert_eq!(run_with(&mut caller, &store), Changed::No);
    }

    #[test]
    fn arity_mismatch_is_left_alone() {
        // `double` reads arg 0; calling it with no operands would
        // fabricate a value. The IR verifier cannot see this (calls are
        // untyped and cross bodies), so the pass is the first place it is
        // observable -- and it declines rather than miscompiling.
        let mut store = GeneratorStore::new();
        store.add_generator(double());

        let mut body = Body::new();
        body.push(OpKind::Call("double".into()), []);

        assert_eq!(run_with(&mut body, &store), Changed::No);
    }

    #[test]
    fn inlines_a_call_inside_a_loop_body() {
        // The callee is spliced into a region that carries its own block
        // arguments; `core.arg` substitution must still reach inside it.
        let mut store = GeneratorStore::new();
        store.add_generator(double());

        let mut loop_body = Body::new();
        let acc = loop_body.push(OpKind::BlockArg(1), []);
        let doubled = loop_body.push(OpKind::Call("double".into()), [acc]);
        loop_body.push(OpKind::Yield, [doubled]);

        let mut body = Body::new();
        let start = body.push(OpKind::Const(Attr::Int(0)), []);
        let end = body.push(OpKind::Const(Attr::Int(3)), []);
        let step = body.push(OpKind::Const(Attr::Int(1)), []);
        let init = body.push(OpKind::Const(Attr::Int(1)), []);
        body.push_with_regions(
            OpKind::For,
            [start, end, step, init],
            [Region::with_args(2, loop_body)],
        );

        assert_eq!(run_with(&mut body, &store), Changed::Yes);
        // The call is gone, replaced by the add, and the yield still
        // terminates the body region.
        assert!(print_body(&body).contains("core.add"));
        assert!(!print_body(&body).contains("core.call"));
        assert!(print_body(&body).contains("cf.yield"));
    }

    #[test]
    fn chain_inlines_under_fixpoint() {
        // `outer(x) = inner(x)`, `inner(x) = x + x`. One pass inlines only
        // the leaf-most legal call (`inner` into `outer` is not attempted
        // here -- the *caller* calls `outer`, which is not call-free); the
        // fixpoint driver is what completes the chain.
        let mut store = GeneratorStore::new();
        store.add_generator(double());
        let mut outer_body = Body::new();
        let a = outer_body.push(OpKind::Arg(0), []);
        outer_body.push(OpKind::Call("double".into()), [a]);
        store.add_generator(Generator {
            name: "outer".into(),
            params: vec![],
            body: outer_body,
        });

        let mut body = Body::new();
        let five = body.push(OpKind::Const(Attr::Int(5)), []);
        body.push(OpKind::Call("outer".into()), [five]);

        // First run: `outer` still contains a call, so nothing happens.
        assert_eq!(run_with(&mut body, &store), Changed::No);

        // But once `outer` is itself cleaned up (as the fixpoint driver
        // would, bottom-up), the chain collapses. Simulate that by
        // inlining `double` into `outer` first.
        let mut outer_body = Body::new();
        let a = outer_body.push(OpKind::Arg(0), []);
        let b = outer_body.push(OpKind::Arg(0), []);
        outer_body.push(OpKind::Add, [a, b]);
        let mut store2 = GeneratorStore::new();
        store2.add_generator(Generator {
            name: "outer".into(),
            params: vec![],
            body: outer_body,
        });

        assert_eq!(run_with(&mut body, &store2), Changed::Yes);
        assert!(!print_body(&body).contains("core.call"));
    }

    #[test]
    fn identity_callee_in_result_position_keeps_its_result() {
        // `id(a) = a` maps the call to a pre-existing caller op, appending
        // nothing -- the `ensure_result` guard is what keeps the body's
        // positional result correct.
        let mut store = GeneratorStore::new();
        let mut id_body = Body::new();
        id_body.push(OpKind::Arg(0), []);
        store.add_generator(Generator {
            name: "id".into(),
            params: vec![],
            body: id_body,
        });

        let mut body = Body::new();
        let a = body.push(OpKind::Const(Attr::Int(9)), []);
        let unused = body.push(OpKind::Const(Attr::Int(100)), []);
        let _ = unused;
        body.push(OpKind::Call("id".into()), [a]);

        assert_eq!(run_with(&mut body, &store), Changed::Yes);
        // The result must still be the value 9, not the stray 100.
        let result = body.result().unwrap();
        assert!(matches!(body.get(result).kind, OpKind::TupleGet(0)));
    }
}
