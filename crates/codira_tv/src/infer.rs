//! Copyright (c) 2026 Omnira CJSC
//!
//! Argument-sort inference: `OpKind::Arg(i)` is untyped in the IR, so the
//! encoder needs to decide, per argument, whether to model it as a Z3
//! integer or boolean variable. Sorts are inferred from **use sites**
//! across all bodies being compared:
//!
//! * operands of arithmetic/comparison/bitwise/shift ops are `Int`;
//! * operands of `And`/`Or`/`Not` and `cf.if` conditions are `Bool`;
//! * `Eq`/`Ne` force both operands to share a sort, propagating whichever side
//!   is already known.
//!
//! Constraints are applied to a shared map until fixpoint (the map only
//! ever grows, so termination is by size). An argument used as *both*
//! `Int` and `Bool` is a genuine type conflict and aborts validation with
//! an `Unsupported` reason. Arguments left unconstrained (e.g. used only
//! as `x == y` between two otherwise-unused arguments, or only as an `If`
//! branch result) default to `Int` -- documented in the crate docs.
//!
//! Inference deliberately does **not** recurse into loop regions or
//! constrain non-argument operands: op-level sort *checking* happens in
//! the encoder, which reports precise `Unsupported` reasons for anything
//! outside the fragment. This pass exists only to pin argument variables
//! before encoding begins.

use codira_mir::{Body, Op, OpId, OpKind};
use rustc_hash::FxHashMap;

/// The two sorts `codira_smt` can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sort {
    Int,
    Bool,
}

/// Infers a sort for every argument referenced by any of `bodies`.
/// Returns `Err(reason)` on an int/bool conflict.
pub(crate) fn infer_arg_sorts(bodies: &[&Body]) -> Result<FxHashMap<u32, Sort>, String> {
    let mut args: FxHashMap<u32, Sort> = FxHashMap::default();
    loop {
        let mut changed = false;
        for body in bodies {
            walk_body(body, &mut args, &mut changed)?;
        }
        if !changed {
            break;
        }
    }
    Ok(args)
}

/// Walks one region body, returning the (possibly still unknown) sort of
/// its result op.
fn walk_body(
    body: &Body,
    args: &mut FxHashMap<u32, Sort>,
    changed: &mut bool,
) -> Result<Option<Sort>, String> {
    let mut local: FxHashMap<OpId, Option<Sort>> = FxHashMap::default();
    for (id, op) in body.iter() {
        let sort = walk_op(body, op, &local, args, changed)?;
        local.insert(id, sort);
    }
    Ok(body
        .result()
        .and_then(|id| local.get(&id).copied().flatten()))
}

/// The already-known sort of an operand, consulting the argument map for
/// `Arg` ops (whose entry in `local` may predate a later constraint).
fn operand_sort(
    body: &Body,
    local: &FxHashMap<OpId, Option<Sort>>,
    args: &FxHashMap<u32, Sort>,
    id: OpId,
) -> Option<Sort> {
    match body.get(id).kind {
        OpKind::Arg(i) => args.get(&i).copied(),
        _ => local.get(&id).copied().flatten(),
    }
}

/// If `id` is an `Arg`, constrain it to `sort` (conflict -> `Err`).
fn require_arg(
    body: &Body,
    id: OpId,
    sort: Sort,
    args: &mut FxHashMap<u32, Sort>,
    changed: &mut bool,
) -> Result<(), String> {
    if let OpKind::Arg(i) = body.get(id).kind {
        match args.get(&i) {
            None => {
                args.insert(i, sort);
                *changed = true;
            }
            Some(existing) if *existing != sort => {
                return Err(format!(
                    "argument {i} is used as both an integer and a boolean"
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn walk_op(
    body: &Body,
    op: &Op,
    local: &FxHashMap<OpId, Option<Sort>>,
    args: &mut FxHashMap<u32, Sort>,
    changed: &mut bool,
) -> Result<Option<Sort>, String> {
    use OpKind::{
        Add, And, Arg, BitAnd, BitOr, BitXor, Const, Div, Eq, Ge, Gt, If, Le, Lt, Mul, Ne, Neg,
        Not, Or, Rem, Shl, Shr, Sub,
    };
    match &op.kind {
        Const(attr) => Ok(match attr {
            codira_mir::Attr::Int(_) => Some(Sort::Int),
            codira_mir::Attr::Bool(_) => Some(Sort::Bool),
            _ => None,
        }),
        Arg(i) => Ok(args.get(i).copied()),

        Add | Sub | Mul | Div | Rem | BitAnd | BitOr | BitXor | Shl | Shr | Neg => {
            for &operand in &op.operands {
                require_arg(body, operand, Sort::Int, args, changed)?;
            }
            Ok(Some(Sort::Int))
        }

        Lt | Le | Gt | Ge => {
            for &operand in &op.operands {
                require_arg(body, operand, Sort::Int, args, changed)?;
            }
            Ok(Some(Sort::Bool))
        }

        And | Or | Not => {
            for &operand in &op.operands {
                require_arg(body, operand, Sort::Bool, args, changed)?;
            }
            Ok(Some(Sort::Bool))
        }

        Eq | Ne => {
            // Propagate whichever operand's sort is already known to the
            // other side; two mutually-unknown arguments stay unresolved
            // (and default to Int after fixpoint).
            let known = op
                .operands
                .iter()
                .find_map(|&o| operand_sort(body, local, args, o));
            if let Some(sort) = known {
                for &operand in &op.operands {
                    require_arg(body, operand, sort, args, changed)?;
                }
            }
            Ok(Some(Sort::Bool))
        }

        If => {
            if let Some(&cond) = op.operands.first() {
                require_arg(body, cond, Sort::Bool, args, changed)?;
            }
            let mut result = None;
            for region in &op.regions {
                let branch = walk_body(&region.body, args, changed)?;
                result = result.or(branch);
            }
            Ok(result)
        }

        // Everything else (loops, calls, tuples, params, block args,
        // yields) is rejected by the encoder with a precise reason; no
        // argument constraints to collect here.
        _ => Ok(None),
    }
}
