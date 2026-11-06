//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Type inference for lazy pipelines (`builtin::PipelineOp`): their sources
//!   (`xs.iter()`, a range, an object with `next(self) -> T?`), adapters and
//!   terminals, and `for` loops over a pipeline or an iterator object.
//! - The checks that need the whole body typed: that a pipeline never leaves
//!   the expression that builds it, that `rev()` has a source it can walk
//!   backwards, that a `for` body does not assign to a variable a stage's
//!   closure reads, and what `sum`, `min` and `max` need of the elements.
//!
//! # Iterator objects
//!
//! A class with a method `next(self) -> T?` (no other parameter) is an
//! iterator of `T`: `for x in it` calls `it.next()` until it returns `nil`,
//! and `it.iter()` starts a pipeline from it. The method is found by name,
//! not through a trait: traits cannot be generic yet, so no `Iterator[T]`
//! trait could say what `T` is. A value struct is refused, because `next`
//! would get a copy of it and could never advance.

use la_arena::ArenaMap;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{error_type, ClosureInfo, Expectation, InferenceDiagnostic, InferenceResultBuilder};
use crate::{
    builtin::PipelineOp,
    code_model::StructMemoryKind,
    expr::{BinaryOp, Expr, ExprId},
    method_resolution::{lookup_method, AssociationMode},
    name::Name,
    ty::{primitives::IntTy, Ty, TyKind},
    BuiltinFn, CallableDef, Function, HirDisplay,
};

/// What a type is, as the source of a `for` loop or a pipeline.
pub(super) enum IteratorObject {
    /// Not a class with a `next` method.
    No,
    /// An iterator of this element type.
    Yields(Ty),
    /// Has a `next` method, but not one that can drive a loop (reported).
    Refused,
}

impl InferenceResultBuilder<'_> {
    /// Whether the method call `call` on `receiver` starts a pipeline over
    /// the pieces of a string: `receiver` is `s.split(sep)` or `s.lines()`,
    /// `call` is a pipeline operation other than `iter`, and the chain of
    /// pipeline operations it starts uses one that arrays do not have
    /// (`count`, `take`, `enumerate`, `find`, ...). Then
    /// `s.split(",").map(f).count()` is one loop over the pieces, with no
    /// array. A chain of only what arrays have (`s.split(",").map(f)`,
    /// `.filter(g).sum()`) keeps the eager array methods it always had, so
    /// no program that compiled before changes meaning; `s.split(",").iter()`
    /// starts a pipeline as on any array (code generation reads the pieces
    /// without the array there too).
    pub(super) fn starts_text_pipeline(&self, call: ExprId, receiver: ExprId) -> bool {
        if !matches!(
            self.builtin_calls.get(&receiver),
            Some(BuiltinFn::String(
                crate::StringOp::Split | crate::StringOp::Lines
            ))
        ) {
            return false;
        }
        let name_of = |expr: ExprId| match &self.body[expr] {
            Expr::MethodCall { method_name, .. } => method_name.as_str().map(str::to_owned),
            _ => None,
        };
        let Some(first) = name_of(call) else {
            return false;
        };
        if first == "iter" || PipelineOp::from_name(&first).is_none() {
            return false;
        }
        // Up the chain: each method call on the one before.
        let mut current = call;
        loop {
            let Some(name) = name_of(current) else {
                return false;
            };
            if PipelineOp::from_name(&name).is_none() {
                return false;
            }
            if crate::ArrayMethod::from_name(&name).is_none() {
                return true;
            }
            let parent = self.body.exprs().find_map(|(id, expr)| match expr {
                Expr::MethodCall { receiver, .. } if *receiver == current => Some(id),
                _ => None,
            });
            match parent {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    /// Infers `start..end` as the receiver of a pipeline operation: a range
    /// is a pipeline of its bounds' integer type.
    pub(super) fn infer_range_pipeline_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        (start, end): (ExprId, ExprId),
        op: PipelineOp,
        args: &[ExprId],
    ) -> Ty {
        let int = self.infer_range_bounds(start, end);
        self.set_expr_type(receiver, TyKind::Iter(int.clone()).intern());
        self.infer_pipeline_method(tgt_expr, op, &int, args)
    }

    /// Infers `receiver.iter()` on an array or an iterator object, or
    /// returns `None` when the receiver is neither (so that the usual
    /// "method not found" is reported).
    pub(super) fn infer_iter_source(
        &mut self,
        tgt_expr: ExprId,
        receiver_ty: &Ty,
        args: &[ExprId],
    ) -> Option<Ty> {
        let elem = match receiver_ty.interned() {
            TyKind::Array(elem) => elem.clone(),
            TyKind::Struct(..) => match self.infer_iterator_object(tgt_expr, receiver_ty) {
                IteratorObject::No => return None,
                IteratorObject::Yields(elem) => elem,
                IteratorObject::Refused => error_type(),
            },
            _ => return None,
        };
        Some(self.infer_pipeline_method(tgt_expr, PipelineOp::Iter, &elem, args))
    }

    /// Whether `ty` is an iterator object (see the module documentation),
    /// recording its `next` method under `key` when it is.
    pub(super) fn infer_iterator_object(&mut self, key: ExprId, ty: &Ty) -> IteratorObject {
        let Some(strukt) = ty.as_struct() else {
            return IteratorObject::No;
        };
        let name = Name::new("next");
        let Ok(next) = lookup_method(
            self.db,
            ty,
            self.module(),
            &name,
            Some(AssociationMode::WithSelf),
        ) else {
            return IteratorObject::No;
        };
        let function = Function::from(next);
        let type_name = ty.display(self.db).to_string();
        let refuse = |this: &mut Self, why: String| {
            this.diagnostics.push(InferenceDiagnostic::ExprError {
                id: key.into(),
                message: why,
            });
            IteratorObject::Refused
        };
        if strukt.data(self.db).memory_kind != StructMemoryKind::Gc {
            return refuse(
                self,
                format!(
                    "`{type_name}` is a value struct, so it cannot be an iterator: `next(self)` \
                     would get a copy and could never advance; declare it as a `class`"
                ),
            );
        }
        let signature = self.db.callable_sig(CallableDef::Function(function));
        let Some(self_ty) = function.self_param_ty(self.db) else {
            return IteratorObject::No;
        };
        let (mut params, ret, fresh) = self.instantiate_signature_with_args(
            &signature
                .params()
                .iter()
                .cloned()
                .chain(std::iter::once(self_ty))
                .collect::<Vec<_>>(),
            signature.ret(),
        );
        let receiver = params.pop().expect("the receiver was appended");
        if !params.is_empty() {
            return refuse(
                self,
                format!(
                    "`{type_name}.next` takes arguments, so it cannot drive a loop: an iterator's \
                     `next` takes only `self` and returns `T?` (`nil` when there are no more)"
                ),
            );
        }
        self.unify(&receiver, ty);
        let ret = self.normalize(&ret);
        let TyKind::Optional(elem) = ret.interned() else {
            return refuse(
                self,
                format!(
                    "`{type_name}.next` returns `{}`, so it cannot drive a loop: an iterator's \
                     `next` returns `T?` (`nil` when there are no more)",
                    ret.display(self.db)
                ),
            );
        };
        let elem = elem.clone();
        self.record_instantiation(key, function, &fresh);
        self.iterator_nexts.insert(key, next);
        IteratorObject::Yields(elem)
    }

    /// Infers the pipeline operation `op` applied to a pipeline of `elem`s.
    pub(super) fn infer_pipeline_method(
        &mut self,
        tgt_expr: ExprId,
        op: PipelineOp,
        elem: &Ty,
        args: &[ExprId],
    ) -> Ty {
        self.builtin_calls.insert(tgt_expr, BuiltinFn::Pipeline(op));
        if args.len() != op.arity() {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: op.arity(),
                });
            for &arg in args {
                self.infer_expr(arg, &Expectation::none());
            }
            return error_type();
        }
        let usize_ty = || TyKind::Int(IntTy::usize()).intern();
        let bool_ty = || TyKind::Bool.intern();
        let iter = |ty: Ty| TyKind::Iter(ty).intern();
        let elem = elem.clone();
        match op {
            PipelineOp::Iter | PipelineOp::Rev => iter(elem),
            PipelineOp::Map => {
                let result = self.type_variables.new_type_var();
                let f = Ty::function(vec![elem], result.clone());
                let f = self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                let result = f
                    .as_function()
                    .map(|(_, ret)| ret.clone())
                    .unwrap_or(result);
                iter(self.resolve_ty_as_far_as_possible(result))
            }
            PipelineOp::Filter | PipelineOp::TakeWhile | PipelineOp::SkipWhile => {
                let f = Ty::function(vec![elem.clone()], bool_ty());
                self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                iter(elem)
            }
            PipelineOp::Take | PipelineOp::Skip => {
                self.infer_expr_coerce(args[0], &Expectation::has_type(usize_ty()));
                iter(elem)
            }
            PipelineOp::Enumerate => {
                iter(TyKind::Tuple(2, [usize_ty(), elem].into_iter().collect()).intern())
            }
            PipelineOp::Zip => {
                let other = self
                    .infer_pipeline_operand(op, args[0])
                    .unwrap_or_else(error_type);
                iter(TyKind::Tuple(2, [elem, other].into_iter().collect()).intern())
            }
            PipelineOp::Chain => {
                let other = self
                    .infer_pipeline_operand(op, args[0])
                    .unwrap_or_else(error_type);
                if !other.is_unknown() && !self.unify(&elem, &other) {
                    let message = format!(
                        "`chain` needs elements of the same type: this pipeline yields `{}`, \
                         the argument `{}`",
                        self.resolve_ty_as_far_as_possible(elem.clone())
                            .display(self.db),
                        self.resolve_ty_as_far_as_possible(other).display(self.db)
                    );
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: args[0].into(),
                        message,
                    });
                }
                iter(elem)
            }
            PipelineOp::Sum => elem,
            PipelineOp::Min | PipelineOp::Max | PipelineOp::Find => {
                if op == PipelineOp::Find {
                    let f = Ty::function(vec![elem.clone()], bool_ty());
                    self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                }
                TyKind::Optional(elem).intern()
            }
            PipelineOp::Position => {
                let f = Ty::function(vec![elem], bool_ty());
                self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                TyKind::Optional(usize_ty()).intern()
            }
            PipelineOp::Any | PipelineOp::All => {
                let f = Ty::function(vec![elem], bool_ty());
                self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                bool_ty()
            }
            PipelineOp::Count => usize_ty(),
            PipelineOp::Fold => {
                let acc = self.infer_expr(args[0], &Expectation::none());
                let f = Ty::function(vec![acc.clone(), elem], acc.clone());
                self.infer_expr_coerce(args[1], &Expectation::has_type(f));
                self.resolve_ty_as_far_as_possible(acc)
            }
            PipelineOp::Collect => TyKind::Array(elem).intern(),
        }
    }

    /// Infers the argument of `zip` or `chain` and returns its element
    /// type: an array, a range, or (`chain` only, any; `zip`, only a bare
    /// `xs.iter()` or `(a..b).iter()`) a pipeline.
    fn infer_pipeline_operand(&mut self, op: PipelineOp, arg: ExprId) -> Option<Ty> {
        if let Expr::Range { start, end, .. } = self.body[arg] {
            let int = self.infer_range_bounds(start, end);
            self.set_expr_type(arg, TyKind::Iter(int.clone()).intern());
            return Some(int);
        }
        let ty = self.infer_expr(arg, &Expectation::none());
        let ty = self.normalize(&ty);
        match ty.interned() {
            TyKind::Array(elem) => Some(elem.clone()),
            TyKind::Iter(elem) if op == PipelineOp::Chain || self.is_bare_source(arg) => {
                Some(elem.clone())
            }
            TyKind::Unknown => None,
            _ => {
                let what = if op == PipelineOp::Zip {
                    "an array, a range or a bare `xs.iter()` (to zip with a pipeline that has \
                     stages, `collect()` it first)"
                } else {
                    "an array, a range or a pipeline"
                };
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: arg.into(),
                    message: format!(
                        "`{}` takes {what}, not `{}`",
                        op.name(),
                        ty.display(self.db)
                    ),
                });
                None
            }
        }
    }

    /// Whether `expr` is a pipeline with no stage that can be read by
    /// index: `xs.iter()` on an array, or a range (with or without
    /// `.iter()`).
    fn is_bare_source(&mut self, expr: ExprId) -> bool {
        let body = self.body;
        match &body[expr] {
            Expr::Range { .. } => true,
            Expr::MethodCall { receiver, .. }
                if self.builtin_calls.get(&expr)
                    == Some(&BuiltinFn::Pipeline(PipelineOp::Iter)) =>
            {
                if matches!(body[*receiver], Expr::Range { .. }) {
                    return true;
                }
                let Some(ty) = self.type_of_expr.get(*receiver).cloned() else {
                    return false;
                };
                matches!(self.normalize(&ty).interned(), TyKind::Array(_))
            }
            _ => false,
        }
    }

    /// After inference: the checks of the module documentation.
    pub(super) fn check_pipelines(
        &mut self,
        expr_types: &ArenaMap<ExprId, Ty>,
        closures: &FxHashMap<ExprId, ClosureInfo>,
    ) {
        let mut ops: Vec<(ExprId, PipelineOp)> = self
            .builtin_calls
            .iter()
            .filter_map(|(&expr, &builtin)| match builtin {
                BuiltinFn::Pipeline(op) => Some((expr, op)),
                _ => None,
            })
            .collect();
        let iterates_object = self.body.exprs().any(|(expr, e)| {
            matches!(e, Expr::For { .. }) && self.iterator_nexts.contains_key(&expr)
        });
        if ops.is_empty() && !iterates_object {
            return;
        }
        ops.sort_by_key(|(expr, _)| u32::from(expr.into_raw()));

        // Where an `Iter[T]` may stand: as the receiver of a pipeline
        // operation, the iterable of a `for` loop, or the argument of
        // `zip` and `chain`.
        let mut consumed: FxHashSet<ExprId> = FxHashSet::default();
        for &(expr, op) in &ops {
            if let Expr::MethodCall { receiver, args, .. } = &self.body[expr] {
                consumed.insert(*receiver);
                if matches!(op, PipelineOp::Zip | PipelineOp::Chain) {
                    consumed.extend(args.first().copied());
                }
            }
        }
        let mut for_loops = Vec::new();
        for (expr, e) in self.body.exprs() {
            if let Expr::For { iterable, body, .. } = e {
                consumed.insert(*iterable);
                for_loops.push((expr, *iterable, *body));
            }
        }
        let is_iter = |expr: ExprId| {
            expr_types
                .get(expr)
                .is_some_and(|ty| matches!(ty.interned(), TyKind::Iter(_)))
        };
        let mut escaping = Vec::new();
        for (expr, ty) in expr_types.iter() {
            let TyKind::Iter(_) = ty.interned() else {
                continue;
            };
            let is_stage = match self.builtin_calls.get(&expr) {
                Some(BuiltinFn::Pipeline(op)) => !op.is_terminal(),
                _ => matches!(self.body[expr], Expr::Range { .. }),
            };
            match &self.body[expr] {
                // A binding of a pipeline was reported where it was bound.
                Expr::Path(_) | Expr::Missing => continue,
                _ if is_stage && consumed.contains(&expr) => continue,
                // Something else of type `Iter[T]` (a block, an `if`, a
                // call) is reported only where it is used as a pipeline;
                // the pipelines inside it are reported themselves.
                _ if !is_stage && !consumed.contains(&expr) => continue,
                _ => {}
            }
            let message = if is_stage {
                format!(
                    "a lazy pipeline (`{}`) exists only inside the expression that builds it: \
                     it cannot be stored in a variable, returned, passed to a function or left \
                     unused. End it in the same chain with a terminal (`sum`, `fold`, `count`, \
                     `any`, `all`, `find`, `position`, `min`, `max`, `collect`) or loop over it \
                     with `for`; `collect()` it to keep the elements",
                    ty.display(self.db)
                )
            } else {
                format!(
                    "a pipeline must be one chain of calls written out in place \
                     (`xs.iter().map(f).sum()`); this `{}` is built somewhere else",
                    ty.display(self.db)
                )
            };
            escaping.push((expr, message));
        }
        for (expr, message) in escaping {
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: expr.into(),
                message,
            });
        }

        for &(expr, op) in &ops {
            let Expr::MethodCall { receiver, .. } = &self.body[expr] else {
                continue;
            };
            let receiver = *receiver;
            if op == PipelineOp::Rev && !self.reversible(receiver, expr_types) {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: expr.into(),
                    message: "`rev()` walks its source from the back, so it needs an array or \
                              a range with only `map` and `filter` before it (`xs.iter().rev()`, \
                              `(0..n).map(f).rev()`); `collect()` first to reverse anything else"
                        .to_string(),
                });
            }
            let Some(requirement) = op.element_requirement() else {
                continue;
            };
            let Some(TyKind::Iter(elem)) = expr_types.get(receiver).map(Ty::interned) else {
                continue;
            };
            if elem.is_unknown() || super::array_methods::element_meets(requirement, elem) {
                continue;
            }
            let what = super::array_methods::requirement_text(requirement);
            let hint = match elem.interned() {
                TyKind::TypeParam(..) => {
                    "; inside a generic function the element type is not known yet, so use \
                     `fold` with a closure that calls a trait method of a bound"
                }
                _ => "; for other types, use `fold` with a closure",
            };
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: expr.into(),
                message: format!(
                    "`{}` needs a pipeline of {what}, not of `{}`{hint}",
                    op.name(),
                    elem.display(self.db)
                ),
            });
        }

        // A closure compiled in place reads the variables it mentions when
        // each element reaches it, not when the pipeline is built; a `for`
        // body assigning one of them would show through. (Elsewhere no user
        // code runs between the stages.)
        for (_, iterable, body) in for_loops {
            if !is_iter(iterable) {
                continue;
            }
            let mut read = FxHashSet::default();
            self.collect_stage_captures(iterable, closures, &mut read);
            if read.is_empty() {
                continue;
            }
            let mut assignments = Vec::new();
            self.collect_assignments(body, &mut assignments);
            for (assignment, lhs) in assignments {
                let Some(pat) = self.place_root_binding(lhs) else {
                    continue;
                };
                if !read.contains(&pat) {
                    continue;
                }
                let name = super::closures::binding_name(self.body, pat);
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: assignment.into(),
                    message: format!(
                        "cannot assign to `{name}` inside this loop: a closure of the pipeline \
                         it iterates reads `{name}`, and runs between the iterations; copy it \
                         into a `let` before the loop, or keep the state in a class instance"
                    ),
                });
            }
        }
    }

    /// Whether the pipeline `expr` (the receiver of a `rev()`) can be walked
    /// from the back: an array or a range, then only `map`, `filter` and
    /// `rev`.
    fn reversible(&self, expr: ExprId, expr_types: &ArenaMap<ExprId, Ty>) -> bool {
        match (&self.body[expr], self.builtin_calls.get(&expr)) {
            (Expr::Range { .. }, _) => true,
            (
                Expr::MethodCall { receiver, .. },
                Some(BuiltinFn::Pipeline(PipelineOp::Map | PipelineOp::Filter | PipelineOp::Rev)),
            ) => self.reversible(*receiver, expr_types),
            (Expr::MethodCall { receiver, .. }, Some(BuiltinFn::Pipeline(PipelineOp::Iter))) => {
                match expr_types.get(*receiver).map(Ty::interned) {
                    Some(TyKind::Array(_)) => true,
                    Some(TyKind::Iter(_)) => self.reversible(*receiver, expr_types),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// The bindings the closure literals of the pipeline `expr` (its
    /// stages, and those of the pipelines it chains) capture.
    fn collect_stage_captures(
        &self,
        expr: ExprId,
        closures: &FxHashMap<ExprId, ClosureInfo>,
        out: &mut FxHashSet<crate::PatId>,
    ) {
        let Some(BuiltinFn::Pipeline(op)) = self.builtin_calls.get(&expr) else {
            return;
        };
        let Expr::MethodCall { receiver, args, .. } = &self.body[expr] else {
            return;
        };
        if let Some(index) = op.closure_arg() {
            if let Some(&closure) = args.get(index) {
                if crate::is_inlined_closure_literal(self.body, closure) {
                    if let Some(info) = closures.get(&closure) {
                        out.extend(info.captures.iter().copied());
                    }
                }
            }
        }
        if *op == PipelineOp::Chain {
            if let Some(&arg) = args.first() {
                self.collect_stage_captures(arg, closures, out);
            }
        }
        self.collect_stage_captures(*receiver, closures, out);
    }

    /// Every assignment (`x = ..`, `x += ..`) in `expr`, outside closures,
    /// with its left-hand side.
    fn collect_assignments(&self, expr: ExprId, out: &mut Vec<(ExprId, ExprId)>) {
        match &self.body[expr] {
            Expr::BinaryOp {
                lhs,
                op: Some(BinaryOp::Assignment { .. }),
                ..
            } => out.push((expr, *lhs)),
            // A closure's own assignments to captures are errors already.
            Expr::Closure { .. } => return,
            _ => {}
        }
        self.body[expr].walk_child_exprs(|child| self.collect_assignments(child, out));
    }
}
