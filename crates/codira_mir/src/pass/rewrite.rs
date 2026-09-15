//! Copyright (c) 2026 Omnira CJSC
//!
//! Shared structural-rewriting machinery for the pass suite: body splicing
//! with block-argument / function-argument substitution, plus the small
//! helpers every rebuilding pass needs (operand renumbering, result-position
//! preservation).
//!
//! This is the load-bearing core of `const-fold`'s `cf.if` inlining,
//! `loop-unroll`'s body duplication, and `inline`'s call-site expansion --
//! the analog of MLIR's `mlir::inlineRegion` / KGEN's region-inlining
//! utilities under `modular/KGEN/lib/Transforms/` (e.g. what `LowerLoops`
//! and the interpreter's region instantiation both lean on). All three
//! passes share one `splice` so the subtle scoping rules below are written
//! -- and tested -- exactly once.
//!
//! # The scoping model (read this before touching anything)
//!
//! `Body` operands are **region-local** arena indices: an op inside a
//! nested [`Region`] can *never* name an op of the enclosing body by
//! operand id. Cross-region dataflow happens exclusively through the
//! nullary reference ops:
//!
//! * [`OpKind::BlockArg`]`(i)` -- names argument `i` of the *innermost
//!   enclosing region carrying arguments* (`num_args > 0`). Regions with
//!   `num_args == 0` (`cf.if` branches) are **transparent**: a `BlockArg`
//!   inside them still refers to the enclosing loop's frame.
//! * [`OpKind::Arg`]`(i)` -- names the enclosing *function*'s runtime argument.
//!   Function-scoped: valid at any region depth, through any number of frames.
//!
//! # What splicing must therefore do
//!
//! When [`splice`] copies a source body into a destination body and a
//! substitution says "`BlockArg(0)` is now value `%v`", there are two
//! representationally different cases:
//!
//! 1. **Top level of the spliced body**: the `BlockArg` op's *def* is simply
//!    dropped and every use is renumbered to `%v` (a real operand id in the
//!    destination -- legal, same region).
//! 2. **Inside a nested region of the spliced body**: operand ids cannot cross
//!    the region boundary, so the substituted value must be *re-materialized*
//!    as a fresh nullary op inside that region. That is only possible when the
//!    value's defining op is itself context-free:
//!
//!    | defining op of `%v`      | re-materializable where?                 |
//!    |--------------------------|------------------------------------------|
//!    | `core.const`, `param.ref`, `core.arg` | anywhere (context-free / function-scoped) |
//!    | `cf.block_arg`           | only while no `num_args > 0` frame has been crossed (same-frame) |
//!    | anything else            | nowhere -- the splice is infeasible       |
//!
//! Substitution *scope* follows the reference ops' own scoping:
//! `block_args` substitution applies at the top level and through
//! transparent (`num_args == 0`) regions only -- a nested region with its
//! own arguments opens a fresh frame whose `BlockArg`s belong to *it*, so
//! substitution stops there. `args` substitution applies at **every**
//! depth (function scope), which is why crossing a frame matters for the
//! `SameFrame` row above.
//!
//! Callers that pass a substitution must call [`can_splice`] first; a
//! `splice` that hits an unsubstitutable reference panics (it would
//! otherwise emit ill-scoped IR, and half-spliced destinations are not
//! recoverable). `const-fold`'s `cf.if` splice uses no substitution at
//! all -- an `If` region is transparent, so its `BlockArg` ops are copied
//! verbatim and keep referring to the same enclosing frame; that is the
//! index-stability argument spelled out in `const_fold.rs`.

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::{
    op::{Body, Op, OpId, OpKind, Region},
    Attr,
};

/// What to substitute while splicing. `None` fields mean "copy those
/// reference ops verbatim".
pub(crate) struct Substitution<'a> {
    /// `BlockArg(i)` (of the spliced body's own frame) becomes
    /// `block_args[i]`, an op id in the destination body. Applies at the
    /// top level and through transparent nested regions; stops at any
    /// nested region with `num_args > 0`.
    pub block_args: Option<&'a [OpId]>,
    /// `Arg(i)` becomes `args[i]`, an op id in the destination body.
    /// Function-scoped: applies at every nesting depth.
    pub args: Option<&'a [OpId]>,
}

impl Substitution<'_> {
    /// No substitution: a pure structural copy (used by `const-fold`'s
    /// `cf.if` splicing).
    pub fn none() -> Substitution<'static> {
        Substitution {
            block_args: None,
            args: None,
        }
    }
}

/// Outcome of a [`splice`].
pub(crate) struct Spliced {
    /// Destination id of the source body's result op (`None` when the
    /// source body was empty, or ended in `cf.yield` -- see `yielded`).
    pub result: Option<OpId>,
    /// When the source body ended in `cf.yield` (a loop body), the yield
    /// op itself is *not* copied -- it is only meaningful as a loop-region
    /// terminator. Instead its operands, renumbered into the destination,
    /// are returned here (the "next carried values" for `loop-unroll`).
    pub yielded: Option<Vec<OpId>>,
}

/// How (whether) a substitution value can be re-created inside a nested
/// region -- see the module doc's table.
enum Remat {
    /// `core.const` / `param.ref` / `core.arg`: context-free or
    /// function-scoped, valid in any region.
    Anywhere(OpKind),
    /// `cf.block_arg` of the destination frame: valid only while every
    /// region on the path is transparent (`num_args == 0`).
    SameFrame(OpKind),
    /// An arbitrary computed value: representable only as a top-level
    /// operand id, never inside a nested region.
    No,
}

fn classify(dst: &Body, id: OpId) -> Remat {
    match &dst.get(id).kind {
        k @ (OpKind::Const(_) | OpKind::Arg(_) | OpKind::ParamRef(_)) => Remat::Anywhere(k.clone()),
        k @ OpKind::BlockArg(_) => Remat::SameFrame(k.clone()),
        _ => Remat::No,
    }
}

/// Checks that [`splice`] with this substitution can produce well-scoped
/// IR: every substituted reference reached inside a nested region must be
/// re-materializable there (module-doc table), and every substituted index
/// must be in bounds. Top-level references are always fine (case 1 in the
/// module doc). Call this before `splice` whenever a substitution is
/// involved; `splice` panics where this returns `false`.
pub(crate) fn can_splice(dst: &Body, src: &Body, subst: &Substitution<'_>) -> bool {
    let ba: Option<Vec<Remat>> = subst
        .block_args
        .map(|ids| ids.iter().map(|&i| classify(dst, i)).collect());
    let ar: Option<Vec<Remat>> = subst
        .args
        .map(|ids| ids.iter().map(|&i| classify(dst, i)).collect());
    body_feasible(src, ba.as_deref(), ar.as_deref(), false, false)
}

fn body_feasible(
    body: &Body,
    ba: Option<&[Remat]>,
    ar: Option<&[Remat]>,
    nested: bool,
    crossed_frame: bool,
) -> bool {
    for (_, op) in body.iter() {
        match &op.kind {
            OpKind::BlockArg(i) => {
                if let Some(ba) = ba {
                    let Some(remat) = ba.get(*i as usize) else {
                        return false; // out-of-bounds substitution index
                    };
                    // `block_args` substitution never crosses a frame (it
                    // is cut at `num_args > 0` regions below), so both
                    // `Anywhere` and `SameFrame` are fine when nested.
                    if nested && matches!(remat, Remat::No) {
                        return false;
                    }
                }
            }
            OpKind::Arg(i) => {
                if let Some(ar) = ar {
                    let Some(remat) = ar.get(*i as usize) else {
                        return false;
                    };
                    if nested {
                        match remat {
                            Remat::Anywhere(_) => {}
                            Remat::SameFrame(_) if !crossed_frame => {}
                            _ => return false,
                        }
                    }
                }
            }
            _ => {}
        }
        for region in &op.regions {
            let transparent = region.num_args == 0;
            let inner_ba = if transparent { ba } else { None };
            let inner_crossed = crossed_frame || !transparent;
            if !body_feasible(&region.body, inner_ba, ar, true, inner_crossed) {
                return false;
            }
        }
    }
    true
}

/// Copies `src` into `dst` (appending), renumbering operands and applying
/// `subst` under the scoping rules in the module doc. A trailing
/// `cf.yield` is consumed rather than copied (see [`Spliced::yielded`]).
///
/// Panics on an unsubstitutable reference -- callers using a substitution
/// must gate on [`can_splice`].
pub(crate) fn splice(dst: &mut Body, src: &Body, subst: &Substitution<'_>) -> Spliced {
    // Snapshot re-materialization kinds *before* mutating `dst` (the
    // substitution targets are pre-existing dst ops, but borrowing rules
    // are simplest with an upfront copy).
    let ba_remat: Option<Vec<Remat>> = subst
        .block_args
        .map(|ids| ids.iter().map(|&i| classify(dst, i)).collect());
    let ar_remat: Option<Vec<Remat>> = subst
        .args
        .map(|ids| ids.iter().map(|&i| classify(dst, i)).collect());

    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let last = src.result();
    let mut yielded = None;

    for (id, op) in src.iter() {
        if Some(id) == last && matches!(op.kind, OpKind::Yield) {
            yielded = Some(op.operands.iter().map(|o| map[o]).collect());
            break;
        }
        match &op.kind {
            OpKind::BlockArg(i) if subst.block_args.is_some() => {
                map.insert(id, subst.block_args.unwrap()[*i as usize]);
            }
            OpKind::Arg(i) if subst.args.is_some() => {
                map.insert(id, subst.args.unwrap()[*i as usize]);
            }
            _ => {
                let operands: SmallVec<[OpId; 2]> = op.operands.iter().map(|o| map[o]).collect();
                let regions: SmallVec<[Region; 0]> = op
                    .regions
                    .iter()
                    .map(|r| rebuild_region(r, ba_remat.as_deref(), ar_remat.as_deref(), false))
                    .collect();
                map.insert(
                    id,
                    dst.push_with_regions(op.kind.clone(), operands, regions),
                );
            }
        }
    }

    Spliced {
        result: last.and_then(|l| map.get(&l).copied()),
        yielded,
    }
}

/// Rebuilds one nested region of a spliced op. Substituted references are
/// re-materialized as fresh nullary ops *inside* the region (case 2 in
/// the module doc) -- operand ids cannot cross the region boundary.
fn rebuild_region(
    region: &Region,
    ba: Option<&[Remat]>,
    ar: Option<&[Remat]>,
    crossed_frame: bool,
) -> Region {
    let transparent = region.num_args == 0;
    // A region carrying its own arguments opens a fresh frame: its
    // `BlockArg`s are *its own* and must not be substituted.
    let ba = if transparent { ba } else { None };
    let crossed_frame = crossed_frame || !transparent;

    // Fast path: nothing to substitute below this point -- the region
    // body is self-contained (region-local operand ids), so a wholesale
    // clone is exact.
    if ba.is_none() && ar.is_none() {
        return region.clone();
    }

    let mut new = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    for (id, op) in region.body.iter() {
        match &op.kind {
            OpKind::BlockArg(i) if ba.is_some() => {
                let kind = match &ba.unwrap()[*i as usize] {
                    // block-arg substitution never crosses a frame, so a
                    // same-frame kind is always still in frame here.
                    Remat::Anywhere(k) | Remat::SameFrame(k) => k.clone(),
                    Remat::No => panic!(
                        "splice: unsubstitutable cf.block_arg reference inside a nested \
                         region (caller must gate on can_splice)"
                    ),
                };
                map.insert(id, new.push(kind, []));
            }
            OpKind::Arg(i) if ar.is_some() => {
                let kind = match &ar.unwrap()[*i as usize] {
                    Remat::Anywhere(k) => k.clone(),
                    Remat::SameFrame(k) if !crossed_frame => k.clone(),
                    _ => panic!(
                        "splice: unsubstitutable core.arg reference inside a nested \
                         region (caller must gate on can_splice)"
                    ),
                };
                map.insert(id, new.push(kind, []));
            }
            _ => {
                let operands: SmallVec<[OpId; 2]> = op.operands.iter().map(|o| map[o]).collect();
                let regions: SmallVec<[Region; 0]> = op
                    .regions
                    .iter()
                    .map(|r| rebuild_region(r, ba, ar, crossed_frame))
                    .collect();
                map.insert(
                    id,
                    new.push_with_regions(op.kind.clone(), operands, regions),
                );
            }
        }
    }
    Region::with_args(region.num_args, new)
}

/// Renumbers an op's operands through an old-id -> new-id map. Every
/// rebuilding pass uses this; a missing entry is a pass bug (operands can
/// only point backwards, so the def was already visited) and panics.
pub(crate) fn remap_operands(op: &Op, map: &FxHashMap<OpId, OpId>) -> SmallVec<[OpId; 2]> {
    op.operands.iter().map(|o| map[o]).collect()
}

/// Guarantees `value` is the *positional* result (last op) of `dst`.
///
/// A body's result is "the last op" (`Body::result`), so a rewrite that
/// replaces the last op's uses with an *earlier* value (e.g. unrolling a
/// trip-count-0 `cf.for` whose value becomes its init operand) would
/// silently change the body's result to whatever op happens to be last.
/// There is no dedicated copy/identity op in this IR, so we materialize
/// the identity `core.tuple_get(0)(core.tuple(v)) == v`: two pure ops,
/// semantically transparent, verifier-clean. `const-fold` deliberately
/// refuses to forward a `tuple_get`-of-`tuple` in result position (same
/// reasoning, other direction), so this wrapper is stable under the
/// cleanup pipeline rather than ping-ponging.
pub(crate) fn ensure_result(dst: &mut Body, value: OpId) -> OpId {
    if dst.result() == Some(value) {
        return value;
    }
    let tuple = dst.push(OpKind::Tuple, [value]);
    dst.push(OpKind::TupleGet(0), [tuple])
}

/// `Some(v)` when `id`'s defining op in `body` is `core.const` with an
/// integer attribute. Shared by `loop-unroll` (trip counts) and
/// `simplify` (identity/strength-reduction constants).
pub(crate) fn const_int(body: &Body, id: OpId) -> Option<i64> {
    match &body.get(id).kind {
        OpKind::Const(Attr::Int(v)) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{print_body, verify_body, Attr, Body, OpKind, Region};

    /// A loop-body-shaped source: `[block_arg 0, block_arg 1, add, yield]`.
    fn loop_body_src() -> Body {
        let mut src = Body::new();
        let iv = src.push(OpKind::BlockArg(0), []);
        let acc = src.push(OpKind::BlockArg(1), []);
        let add = src.push(OpKind::Add, [acc, iv]);
        src.push(OpKind::Yield, [add]);
        src
    }

    #[test]
    fn splice_substitutes_top_level_block_args_and_consumes_yield() {
        let src = loop_body_src();
        let mut dst = Body::new();
        let carried = dst.push(OpKind::Const(Attr::Int(7)), []);
        let iv = dst.push(OpKind::Const(Attr::Int(3)), []);

        let subst = Substitution {
            block_args: Some(&[iv, carried]),
            args: None,
        };
        assert!(can_splice(&dst, &src, &subst));
        let spliced = splice(&mut dst, &src, &subst);

        // The two block_arg defs were dropped, the add was renumbered to
        // the destination values, and the yield became `yielded`.
        assert_eq!(
            print_body(&dst),
            "%0 = core.const 7\n%1 = core.const 3\n%2 = core.add %0, %1\n"
        );
        let yielded = spliced.yielded.expect("source ended in yield");
        assert_eq!(yielded.len(), 1);
        assert_eq!(dst.result(), Some(yielded[0]));
        assert!(
            spliced.result.is_none(),
            "yield-terminated: no plain result"
        );
        verify_body(&dst).unwrap();
    }

    #[test]
    fn splice_rematerializes_const_inside_transparent_region() {
        // src: `[const true, if { then: [block_arg 0, const 1, add] else: [const 0] }]`
        let mut src = Body::new();
        let cond = src.push(OpKind::Const(Attr::Bool(true)), []);
        let mut then_b = Body::new();
        let ba = then_b.push(OpKind::BlockArg(0), []);
        let one = then_b.push(OpKind::Const(Attr::Int(1)), []);
        then_b.push(OpKind::Add, [ba, one]);
        let mut else_b = Body::new();
        else_b.push(OpKind::Const(Attr::Int(0)), []);
        src.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );

        let mut dst = Body::new();
        let iv = dst.push(OpKind::Const(Attr::Int(5)), []);
        let subst = Substitution {
            block_args: Some(&[iv]),
            args: None,
        };
        assert!(can_splice(&dst, &src, &subst));
        let spliced = splice(&mut dst, &src, &subst);

        // The block_arg inside the transparent then-region was replaced
        // by a re-materialized `const 5` *inside that region*.
        let if_op = dst.get(spliced.result.unwrap());
        let then_ops: Vec<_> = if_op.regions[0]
            .body
            .iter()
            .map(|(_, o)| o.kind.clone())
            .collect();
        assert_eq!(then_ops[0], OpKind::Const(Attr::Int(5)));
        verify_body(&dst).unwrap();
    }

    #[test]
    fn splice_stops_block_arg_substitution_at_frame_boundary() {
        // src: `[block_arg 0, c, c, for(...) { body(1): [block_arg 0, yield] }]`
        // -- the outer block_arg is substituted, the inner one (a fresh
        // frame: num_args == 1) must survive untouched.
        let mut src = Body::new();
        let outer = src.push(OpKind::BlockArg(0), []);
        let end = src.push(OpKind::Const(Attr::Int(4)), []);
        let step = src.push(OpKind::Const(Attr::Int(1)), []);
        let mut inner = Body::new();
        inner.push(OpKind::BlockArg(0), []);
        inner.push(OpKind::Yield, []);
        src.push_with_regions(
            OpKind::For,
            [outer, end, step],
            [Region::with_args(1, inner)],
        );

        let mut dst = Body::new();
        let start = dst.push(OpKind::Const(Attr::Int(0)), []);
        let subst = Substitution {
            block_args: Some(&[start]),
            args: None,
        };
        assert!(can_splice(&dst, &src, &subst));
        let spliced = splice(&mut dst, &src, &subst);

        let for_op = dst.get(spliced.result.unwrap());
        // Outer use renumbered to the destination const...
        assert_eq!(for_op.operands[0], start);
        // ...inner frame's block_arg untouched.
        let (_, first_inner) = for_op.regions[0].body.iter().next().unwrap();
        assert_eq!(first_inner.kind, OpKind::BlockArg(0));
        verify_body(&dst).unwrap();
    }

    #[test]
    fn can_splice_rejects_computed_value_in_nested_region() {
        // A block_arg used inside a transparent region cannot be
        // substituted with an arbitrary computed value (no way to name it
        // across the region boundary).
        let mut src = Body::new();
        let cond = src.push(OpKind::Const(Attr::Bool(true)), []);
        let mut then_b = Body::new();
        then_b.push(OpKind::BlockArg(0), []);
        let mut else_b = Body::new();
        else_b.push(OpKind::Const(Attr::Unit), []);
        src.push_with_regions(
            OpKind::If,
            [cond],
            [Region::new(then_b), Region::new(else_b)],
        );

        let mut dst = Body::new();
        let a = dst.push(OpKind::Arg(0), []);
        let computed = dst.push(OpKind::Add, [a, a]);
        // Computed value: infeasible...
        assert!(!can_splice(
            &dst,
            &src,
            &Substitution {
                block_args: Some(&[computed]),
                args: None
            }
        ));
        // ...but the function-scoped `core.arg` itself is fine anywhere.
        assert!(can_splice(
            &dst,
            &src,
            &Substitution {
                block_args: Some(&[a]),
                args: None
            }
        ));
    }

    #[test]
    fn can_splice_rejects_block_arg_arg_substitution_across_frame() {
        // `core.arg` used inside a callee *loop* region can be
        // substituted by a caller `cf.block_arg` only if no frame is
        // crossed -- a loop region crosses one.
        let mut src = Body::new();
        let start = src.push(OpKind::Const(Attr::Int(0)), []);
        let end = src.push(OpKind::Const(Attr::Int(3)), []);
        let step = src.push(OpKind::Const(Attr::Int(1)), []);
        let mut inner = Body::new();
        inner.push(OpKind::Arg(0), []);
        inner.push(OpKind::Yield, []);
        src.push_with_regions(
            OpKind::For,
            [start, end, step],
            [Region::with_args(1, inner)],
        );

        let mut dst = Body::new();
        let ba = dst.push(OpKind::BlockArg(0), []);
        assert!(!can_splice(
            &dst,
            &src,
            &Substitution {
                block_args: None,
                args: Some(&[ba])
            }
        ));
        let c = dst.push(OpKind::Const(Attr::Int(9)), []);
        assert!(can_splice(
            &dst,
            &src,
            &Substitution {
                block_args: None,
                args: Some(&[c])
            }
        ));
    }

    #[test]
    fn ensure_result_wraps_non_last_values() {
        let mut body = Body::new();
        let first = body.push(OpKind::Const(Attr::Int(1)), []);
        let last = body.push(OpKind::Const(Attr::Int(2)), []);

        // Already last: untouched.
        assert_eq!(ensure_result(&mut body, last), last);
        assert_eq!(body.len(), 2);

        // Earlier value: wrapped in the tuple/tuple_get identity.
        let wrapped = ensure_result(&mut body, first);
        assert_eq!(body.result(), Some(wrapped));
        assert_eq!(
            print_body(&body),
            "%0 = core.const 1\n%1 = core.const 2\n%2 = core.tuple %0\n%3 = core.tuple_get %2[0]\n"
        );
        verify_body(&body).unwrap();
    }
}
