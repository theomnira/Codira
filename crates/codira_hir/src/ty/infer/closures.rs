//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: October 14, 2026
//!
//! Functionality:
//! - Type inference for function values: closure literals, named functions used
//!   as values, and calls through a value of function type.
//! - Capture analysis: which locals of the enclosing function a closure
//!   mentions, and the by-value capture rule that goes with it.
//!
//! # The capture rule
//!
//! A closure captures every local binding of an enclosing function (or
//! enclosing closure) that its body mentions, *by value, when the closure is
//! created*. The closure works on its own copy: assigning to the variable
//! later, outside, does not change what the closure sees, and the closure
//! itself may not assign to a captured variable (or to a field of a captured
//! value struct) at all -- that would only change the copy, which is never
//! what the writer meant. State that a closure should change lives in a class
//! instance: capturing a class reference copies the reference, so both sides
//! see the same object, which is how a counter generator is written.
//!
//! # Named functions as values
//!
//! A path naming a function has the function's own `FnDef` type where it is
//! called. Anywhere else it becomes a value of type `func(..) -> R`; the
//! expression is recorded in `fn_values` so code generation knows to build a
//! function value for it.

use rustc_hash::FxHashSet;

use super::{error_type, Expectation, InferenceDiagnostic, InferenceResultBuilder};
use crate::{
    code_model::StructMemoryKind,
    expr::{Expr, ExprId, Pat, PatId, Statement},
    resolve::{resolver_for_expr, ValueNs},
    ty::{Ty, TyKind},
    type_ref::LocalTypeRefId,
    HirDisplay,
};

/// What inference found out about one closure literal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClosureInfo {
    /// The bindings of enclosing code the closure captures, in the order of
    /// their first mention in its body.
    pub captures: Vec<PatId>,
    /// The parameter types.
    pub params: Vec<Ty>,
    /// The return type.
    pub ret: Ty,
}

/// A closure whose body is being inferred.
pub(super) struct ClosureFrame {
    /// Every pattern bound inside the closure (its parameters included).
    bound: FxHashSet<PatId>,
    captures: Vec<PatId>,
}

impl InferenceResultBuilder<'_> {
    /// Infers a closure literal against `expected`, which supplies the types
    /// of parameters and the return type that are not written out.
    pub(super) fn infer_closure(
        &mut self,
        tgt_expr: ExprId,
        params: &[(PatId, Option<LocalTypeRefId>)],
        ret_type: Option<LocalTypeRefId>,
        body: ExprId,
        expected: &Expectation,
    ) -> Ty {
        let expected = self.normalize(&expected.ty);
        let expected_sig = expected
            .as_function()
            .filter(|(expected_params, _)| expected_params.len() == params.len())
            .map(|(p, r)| (p.to_vec(), r.clone()));

        let strip_alias = |db: &dyn crate::HirDatabase, ty: Ty| match ty.interned() {
            TyKind::TypeAlias(alias) => alias.target_type(db),
            _ => ty,
        };

        let mut param_tys = Vec::with_capacity(params.len());
        for (idx, &(pat, type_ref)) in params.iter().enumerate() {
            let ty = if let Some(type_ref) = type_ref {
                let ty = self.resolve_type(type_ref);
                strip_alias(self.db, ty)
            } else if let Some((expected_params, _)) = &expected_sig {
                expected_params[idx].clone()
            } else {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: pat.into(),
                    message: "cannot infer the type of this closure parameter: write it out \
                              (`x: T`), or pass the closure where a function type is expected"
                        .to_string(),
                });
                error_type()
            };
            self.infer_pat(pat, ty.clone());
            param_tys.push(ty);
        }

        let ret = if let Some(type_ref) = ret_type {
            let ty = self.resolve_type(type_ref);
            strip_alias(self.db, ty)
        } else if let Some((_, expected_ret)) = &expected_sig {
            expected_ret.clone()
        } else {
            Ty::unit()
        };

        // The body is a function of its own: its own return type, no
        // enclosing loop to `break` out of, no handler arm to `resume`.
        let saved_return = std::mem::replace(&mut self.return_ty, ret.clone());
        let saved_loop = self.active_loop.take();
        let saved_arm = self.active_arm_ret.take();
        let mut bound = FxHashSet::default();
        for &(pat, _) in params {
            self.collect_bound_pats(pat, &mut bound);
        }
        self.collect_pats_bound_in(body, &mut bound);
        self.closure_frames.push(ClosureFrame {
            bound,
            captures: Vec::new(),
        });

        self.infer_expr_coerce(body, &Expectation::has_type(ret.clone()));

        let frame = self.closure_frames.pop().expect("pushed above");
        self.return_ty = saved_return;
        self.active_loop = saved_loop;
        self.active_arm_ret = saved_arm;

        self.closures.insert(
            tgt_expr,
            ClosureInfo {
                captures: frame.captures,
                params: param_tys.clone(),
                ret: ret.clone(),
            },
        );

        Ty::function(param_tys, ret)
    }

    /// Records that `pat` is mentioned: every closure being inferred that
    /// does not bind it itself captures it.
    pub(super) fn record_local_use(&mut self, pat: PatId) {
        for frame in self.closure_frames.iter_mut().rev() {
            if frame.bound.contains(&pat) {
                // Bound inside this closure, so inside every closure it is
                // nested in too.
                break;
            }
            if !frame.captures.contains(&pat) {
                frame.captures.push(pat);
            }
        }
    }

    /// Reports an assignment, inside a closure, to a variable the closure
    /// captured (or to a field of a captured value struct).
    pub(super) fn check_captured_assignment(&mut self, assignment: ExprId, lhs: ExprId) {
        let Some(frame) = self.closure_frames.last() else {
            return;
        };
        let Some(pat) = self.place_root_binding(lhs) else {
            return;
        };
        if frame.bound.contains(&pat) {
            return;
        }
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: assignment.into(),
            message: format!(
                "cannot assign to `{name}` inside this closure: the closure captured `{name}` \
                 by value when it was created, so the assignment would change only the \
                 closure's copy; keep state the closure changes in a class instance and \
                 capture that",
                name = self.binding_name(pat)
            ),
        });
    }

    /// Reports an assignment to a `let` binding, or to a field of a value
    /// struct (or tuple) held in one: `let` binds once. A field of a class
    /// and an array element are storage of a shared object, not of the
    /// binding, and stay assignable through a `let`.
    pub(super) fn check_immutable_assignment(&mut self, assignment: ExprId, lhs: ExprId) {
        let Some(pat) = self.place_root_binding(lhs) else {
            return;
        };
        if !self.body.is_immutable_binding(pat) {
            return;
        }
        let name = self.binding_name(pat);
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: assignment.into(),
            message: format!(
                "cannot assign to `{name}`: it is bound with `let`, which cannot be \
                 assigned after its declaration (not even to give it a first value); \
                 declare it with `var {name}`"
            ),
        });
    }

    /// The local binding whose own storage the place `expr` names: the
    /// variable itself, or a field of a value struct (or tuple) stored in
    /// it. A field of a class or an array element is storage of a shared
    /// object, not of the variable, so it has no root binding here.
    fn place_root_binding(&self, expr: ExprId) -> Option<PatId> {
        match &self.body[expr] {
            Expr::Path(path) => {
                let resolver = resolver_for_expr(self.db, self.body.owner(), expr);
                match resolver.resolve_path_as_value_fully(self.db, path)? {
                    (ValueNs::LocalBinding(pat), _) => Some(pat),
                    _ => None,
                }
            }
            Expr::Field { expr: receiver, .. } => {
                let receiver_ty = self.type_of_expr.get(*receiver)?;
                match receiver_ty.interned() {
                    TyKind::Struct(s, _) if s.data(self.db).memory_kind == StructMemoryKind::Gc => {
                        None
                    }
                    _ => self.place_root_binding(*receiver),
                }
            }
            _ => None,
        }
    }

    /// The written name of the binding `pat`, for a diagnostic.
    fn binding_name(&self, pat: PatId) -> String {
        binding_name(self.body, pat)
    }

    /// Reports every capture whose (fully resolved) type a closure cannot
    /// hold. Run once all types are known, so an integer variable that
    /// defaults later is not mistaken for something uncapturable.
    pub(super) fn check_capture_types(
        &mut self,
        closures: &rustc_hash::FxHashMap<ExprId, ClosureInfo>,
        pat_types: &la_arena::ArenaMap<PatId, Ty>,
    ) {
        let mut closures: Vec<_> = closures.iter().collect();
        closures.sort_by_key(|(expr, _)| expr.into_raw());
        for (&expr, info) in closures {
            for &capture in &info.captures {
                let Some(ty) = pat_types.get(capture) else {
                    continue;
                };
                if is_capturable(ty) || ty.is_unknown() {
                    continue;
                }
                let reason = if matches!(ty.interned(), TyKind::Session(..)) {
                    "a session endpoint is linear and lives only as long as its `scope`, so a \
                     closure cannot keep it"
                } else {
                    "a channel is a handle that lives only as long as its `scope`, so a closure \
                     cannot keep it"
                };
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: expr.into(),
                    message: format!(
                        "this closure cannot capture `{}` of type `{}`: {reason}",
                        binding_name(self.body, capture),
                        ty.display(self.db)
                    ),
                });
            }
        }
    }

    fn collect_bound_pats(&self, pat: PatId, out: &mut FxHashSet<PatId>) {
        out.insert(pat);
        self.body[pat].walk_child_pats(|child| self.collect_bound_pats(child, out));
    }

    /// Every pattern bound anywhere inside `expr`.
    fn collect_pats_bound_in(&self, expr: ExprId, out: &mut FxHashSet<PatId>) {
        match &self.body[expr] {
            Expr::Block { statements, .. } => {
                for statement in statements {
                    if let Statement::Let { pat, .. } = statement {
                        self.collect_bound_pats(*pat, out);
                    }
                }
                if let Some(ghosts) = self.body.contracts().ghost_lets.get(&expr) {
                    for (_, statement) in ghosts {
                        if let Statement::Let { pat, .. } = statement {
                            self.collect_bound_pats(*pat, out);
                        }
                    }
                }
            }
            Expr::For { pat, .. } => self.collect_bound_pats(*pat, out),
            // Also what `if let` and `while let` lower to.
            Expr::Match { arms, .. } => {
                for arm in arms {
                    self.collect_bound_pats(arm.pat, out);
                }
            }
            Expr::Handle { arms, .. } => {
                for arm in arms {
                    for &param in &arm.params {
                        self.collect_bound_pats(param, out);
                    }
                }
            }
            Expr::Closure { params, .. } => {
                for &(param, _) in params {
                    self.collect_bound_pats(param, out);
                }
            }
            _ => {}
        }
        self.body[expr].walk_child_exprs(|child| self.collect_pats_bound_in(child, out));
    }

    /// Turns the type of a path that named a function into a function value,
    /// when the path is not the callee of a call. Returns the type the path
    /// has as a value.
    pub(super) fn function_item_as_value(&mut self, path_expr: ExprId, ty: Ty) -> Ty {
        let TyKind::FnDef(def, _) = ty.interned() else {
            return ty;
        };
        // A tuple struct named without arguments is already reported as a
        // struct literal of the wrong kind; it does not become a value.
        let crate::CallableDef::Function(function) = *def else {
            return ty;
        };
        let data = function.data(self.db);
        let name = function.name(self.db);
        let refusal = if function.is_generic(self.db) {
            Some(format!(
                "the generic function `{name}` cannot be used as a function value yet: a value \
                 names no instantiation of it; wrap the call in a closure instead"
            ))
        } else if !data.effects().is_empty() {
            Some(format!(
                "`{name}` uses effects, so it cannot be used as a function value: a function \
                 value carries no effect handlers"
            ))
        } else if function.is_intrinsic(self.db) || function.is_inline_asm(self.db) {
            Some(format!(
                "`{name}` is emitted in place at each call and has no address, so it cannot be \
                 used as a function value"
            ))
        } else {
            None
        };
        if let Some(message) = refusal {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: path_expr.into(),
                message,
            });
            return error_type();
        }
        let sig = self.db.callable_sig(*def);
        self.fn_values.insert(path_expr, function.id);
        Ty::function(sig.params().to_vec(), sig.ret().clone())
    }

    /// Checks the arguments of a call through a function value of type
    /// `callee_ty` and returns the call's type.
    pub(super) fn infer_indirect_call(
        &mut self,
        tgt_expr: ExprId,
        callee_ty: &Ty,
        args: &[ExprId],
    ) -> Ty {
        let (params, ret) = callee_ty
            .as_function()
            .map(|(p, r)| (p.to_vec(), r.clone()))
            .expect("only function types are called indirectly");
        self.check_call_argument_count(tgt_expr, false, args.len(), params.len());
        for (&arg, param_ty) in args.iter().zip(params.iter()) {
            self.infer_expr_coerce(arg, &Expectation::has_type(param_ty.clone()));
        }
        for &extra in args.iter().skip(params.len()) {
            self.infer_expr(extra, &Expectation::none());
        }
        self.indirect_calls.insert(tgt_expr);
        ret
    }
}

fn binding_name(body: &crate::expr::Body, pat: PatId) -> String {
    match &body[pat] {
        Pat::Bind { name } => name.to_string(),
        _ => "_".to_string(),
    }
}

/// Whether a closure can capture a value of type `ty`: the environment is a
/// garbage-collected object, so every capture needs runtime type
/// information, which a channel (an opaque runtime handle) does not have.
fn is_capturable(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Int(_)
            | TyKind::Float(_)
            | TyKind::Bool
            | TyKind::Struct(..)
            | TyKind::Array(_)
            | TyKind::Function(_)
            | TyKind::Tuple(..)
            | TyKind::Optional(_)
            | TyKind::Enum(..)
            | TyKind::Str
            // Concrete in every instance of a generic function.
            | TyKind::TypeParam(..)
    )
}
