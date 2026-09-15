//! The elaborator: generator specialization (monomorphization).
//!
//! Structural analog of `KGEN/lib/Elaborator` -- see
//! `spec/EIDOS_ARCHITECTURE.md` §4 point 2. Given a
//! [`codira_mir::Generator`] and concrete values for its compile-time
//! parameters, produces a new [`Body`] with every `param.ref` replaced by
//! the concrete value, every `core.arg` (a genuine runtime value) left
//! untouched, and -- unlike a naive substitution pass -- every
//! sub-expression that becomes fully closed over constants after
//! substitution folded down to a single `Const` op. This mirrors KGEN's
//! own pre/post-elaboration `SCCP`/`Canonicalizer` passes
//! (`modular/KGEN/docs/MojoCompilerWalkthrough.md` §"Pre-Elaboration
//! Optimization": "Fewer, smaller, simpler generators are faster to
//! instantiate") -- the point isn't just correctness, it's that the
//! elaborated body should do less work at codegen/runtime than a literal
//! copy-and-substitute would produce.
//!
//! Constant folding delegates to [`codira_mir::fold_op`] directly -- the
//! single place that defines what `+`/`==`/`<<`/etc. mean at compile time,
//! shared with the interpreter (`crate::interp`) and the pass pipeline.
//! `fold_op` returning `NotFoldable` (control flow, tuples, calls,
//! references) or a genuine evaluation error (a constant division by
//! zero) both leave the op in place: elaboration never converts a
//! would-be runtime error into a silently different program -- the
//! interpreter or codegen surfaces it at its real evaluation point.
//!
//! Scope notes:
//! * Loop regions (`cf.while`/`cf.for`) are elaborated recursively --
//!   `param.ref`s inside loop bodies fold like anywhere else -- with their
//!   region `num_args` preserved exactly (the block-arg contract is part of the
//!   loop op's signature, see `codira_mir::Region`).
//! * Tuple-producing ops stay unfolded: `Value::Tuple` has no `Attr` form (see
//!   `crate::value`), so a parameter bound to a tuple also leaves its
//!   `param.ref` in place for the interpreter's `Env` to resolve instead.

use codira_mir::{fold_op, Attr, Body, Generator, Op, OpId, OpKind, Region};
use rustc_hash::{FxHashMap, FxHashSet};
use smol_str::SmolStr;

use crate::{value::Value, Env};

/// Elaborates `generator`'s body under `bindings` (name -> concrete value
/// for each compile-time parameter the caller has resolved; parameters not
/// present in `bindings` are left as `param.ref` -- partial specialization
/// is allowed, matching KGEN's parameters being independently bindable).
pub fn elaborate(generator: &Generator, bindings: &[(SmolStr, Value)]) -> Body {
    let mut env = Env::new();
    for (name, value) in bindings {
        env.bind(name.clone(), value.clone());
    }
    let elaborated = Elaborator { env: &env }.elaborate_body(&generator.body);
    compact(&elaborated)
}

/// Dead-code elimination: keeps only ops reachable from `body`'s result
/// (transitively through operands and, recursively, through the bodies of
/// any regions those ops carry), renumbering as it goes. Substitution and
/// folding above leave the ops they replaced behind in the arena (it's
/// append-only) -- this pass is what actually makes the "fewer ops than
/// naive substitution" claim true of the *returned* body, not just of the
/// values it would compute if you bothered to walk it.
///
/// Region handling: each nested region is compacted with its own
/// `result()` as the liveness root. For loop body regions that root is the
/// terminating `cf.yield` (it is by contract the last op), so exactly the
/// yield-reachable ops survive; and `num_args` is carried over unchanged
/// -- a region's block-argument count is part of its enclosing op's
/// signature and must survive any rewrite (resetting it to 0, as a bare
/// `Region::new` would, breaks the verifier's `cf.while`/`cf.for` arity
/// rules).
fn compact(body: &Body) -> Body {
    let Some(root) = body.result() else {
        return Body::new();
    };

    let mut reachable: FxHashSet<OpId> = FxHashSet::default();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !reachable.insert(id) {
            continue;
        }
        for operand in &body.get(id).operands {
            stack.push(*operand);
        }
    }

    let mut new_body = Body::new();
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    for (old_id, op) in body.iter() {
        if !reachable.contains(&old_id) {
            continue;
        }
        let operands: Vec<OpId> = op.operands.iter().map(|id| map[id]).collect();
        let regions: Vec<Region> = op
            .regions
            .iter()
            .map(|r| Region::with_args(r.num_args, compact(&r.body)))
            .collect();
        let new_id = new_body.push_with_regions(op.kind.clone(), operands, regions);
        map.insert(old_id, new_id);
    }
    new_body
}

struct Elaborator<'a> {
    env: &'a Env,
}

impl Elaborator<'_> {
    fn elaborate_body(&self, old: &Body) -> Body {
        let mut new_body = Body::new();
        let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
        for (old_id, op) in old.iter() {
            let new_id = self.elaborate_op(op, &map, &mut new_body);
            map.insert(old_id, new_id);
        }
        new_body
    }

    fn elaborate_op(&self, op: &Op, map: &FxHashMap<OpId, OpId>, new_body: &mut Body) -> OpId {
        match &op.kind {
            OpKind::ParamRef(name) => match self.env.lookup(name) {
                // A bound scalar parameter becomes a constant; a bound
                // *tuple* parameter has no `Attr` form (see module doc)
                // and stays a `param.ref` for the interpreter to resolve.
                Ok(value) => match value_to_attr(&value) {
                    Some(attr) => new_body.push(OpKind::Const(attr), []),
                    None => new_body.push(OpKind::ParamRef(name.clone()), []),
                },
                // Not bound (partial specialization): pass the reference
                // through unresolved, exactly as written.
                Err(_) => new_body.push(OpKind::ParamRef(name.clone()), []),
            },

            OpKind::Const(_) | OpKind::Arg(_) | OpKind::BlockArg(_) => {
                new_body.push(op.kind.clone(), [])
            }

            OpKind::If => {
                let cond_new = map[&op.operands[0]];
                let then_region = Region::new(self.elaborate_body(&op.regions[0].body));
                let else_region = Region::new(self.elaborate_body(&op.regions[1].body));

                match const_attr(new_body, cond_new) {
                    Some(Attr::Bool(cond)) => {
                        let chosen = if cond { then_region } else { else_region };
                        splice(&chosen.body, new_body)
                    }
                    _ => new_body.push_with_regions(
                        OpKind::If,
                        [cond_new],
                        [then_region, else_region],
                    ),
                }
            }

            // `cf.for`/`cf.while`: recurse into the regions so
            // `param.ref`s inside loop bodies elaborate like anywhere
            // else, preserving each region's `num_args` (the loop op's
            // block-argument contract) and its `cf.yield` terminator
            // (`Yield` elaborates as a plain operand-remap below only
            // when reached through this recursion, and stays last because
            // elaboration preserves op order). Trip-count-known loops are
            // *not* unrolled here -- interpretation (`crate::interp`)
            // executes them, and unrolling as an optimization belongs to
            // the pass pipeline, not the elaborator.
            OpKind::For | OpKind::While => {
                let regions: Vec<Region> = op
                    .regions
                    .iter()
                    .map(|r| Region::with_args(r.num_args, self.elaborate_body(&r.body)))
                    .collect();
                let operands: Vec<OpId> = op.operands.iter().map(|id| map[id]).collect();
                new_body.push_with_regions(op.kind.clone(), operands, regions)
            }

            // Every other op kind is region-free: remap operands and, if
            // it's a pure computation whose operands are now all
            // constant, fold it via the shared fold hook. `NotFoldable`
            // kinds (`core.tuple`, `core.call`, `cf.yield`, ...) and
            // genuine evaluation errors both leave the op as-is -- see
            // the module doc.
            other_kind => {
                let operands: Vec<OpId> = op.operands.iter().map(|id| map[id]).collect();
                if let Some(folded) = try_fold(new_body, other_kind, &operands) {
                    new_body.push(OpKind::Const(folded), [])
                } else {
                    new_body.push(other_kind.clone(), operands)
                }
            }
        }
    }
}

/// If `id` names a `Const` op in `body`, returns its attribute.
fn const_attr(body: &Body, id: OpId) -> Option<Attr> {
    match &body.get(id).kind {
        OpKind::Const(attr) => Some(attr.clone()),
        _ => None,
    }
}

/// Attempts to constant-fold `kind` applied to `operands`, all of which
/// must already be `Const` ops in `body`, by calling
/// [`codira_mir::fold_op`] on their attributes directly. `None` covers
/// "some operand isn't constant yet", "this kind isn't a pure fold"
/// (`NotFoldable`), and "the fold is a genuine evaluation error" alike --
/// in every case the right elaboration move is the same: keep the op.
fn try_fold(body: &Body, kind: &OpKind, operands: &[OpId]) -> Option<Attr> {
    let attrs: Vec<Attr> = operands
        .iter()
        .map(|id| const_attr(body, *id))
        .collect::<Option<Vec<_>>>()?;
    // Refuse to "fold" to an unresolved parameter: `Const(ParamRef)` is a
    // legal attr shape, but folding `Const(@N)` operands would just move
    // the unresolvedness around. (Only `Const(attr)` with zero operands
    // can hit this; every arithmetic fold on a ParamRef operand already
    // fails inside `fold_op` with a type mismatch.)
    if attrs.iter().any(|attr| matches!(attr, Attr::ParamRef(_))) {
        return None;
    }
    fold_op(kind, &attrs).ok()
}

/// Copies every op in `src` into `dst`, remapping operand references and
/// preserving nested regions' `num_args` (same signature-preservation
/// argument as in [`compact`]), and returns the id of the final (last) op
/// in `dst`'s numbering -- used to inline a taken `if`/`else` branch
/// directly into its parent body once the condition has folded to a
/// constant.
fn splice(src: &Body, dst: &mut Body) -> OpId {
    let mut map: FxHashMap<OpId, OpId> = FxHashMap::default();
    let mut last = None;
    for (old_id, op) in src.iter() {
        let operands: Vec<OpId> = op.operands.iter().map(|id| map[id]).collect();
        let regions: Vec<Region> = op
            .regions
            .iter()
            .map(|r| {
                let mut sub = Body::new();
                splice(&r.body, &mut sub);
                Region::with_args(r.num_args, sub)
            })
            .collect();
        let new_id = dst.push_with_regions(op.kind.clone(), operands, regions);
        map.insert(old_id, new_id);
        last = Some(new_id);
    }
    match last {
        Some(id) => id,
        // Empty branch (e.g. `if cond { .. }` with no `else`): its value is
        // Unit, matching the interpreter's `OpKind::If` semantics.
        None => dst.push(OpKind::Const(Attr::Unit), []),
    }
}

/// Converts a concrete scalar [`Value`] to its [`Attr`] form. `None` for
/// tuples, which have no attribute representation (see module doc).
fn value_to_attr(value: &Value) -> Option<Attr> {
    match value {
        Value::Int(v) => Some(Attr::Int(*v)),
        Value::Bool(v) => Some(Attr::Bool(*v)),
        Value::Float(v) => Some(Attr::float(*v)),
        Value::Str(s) => Some(Attr::Str(s.clone())),
        Value::Unit => Some(Attr::Unit),
        Value::Tuple(_) => None,
    }
}
