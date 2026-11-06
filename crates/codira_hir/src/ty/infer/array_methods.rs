//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Type inference for the builtin methods of arrays (`push`, `pop`, `sort`,
//!   `map`, ...; see `builtin::ArrayMethod`) and for slicing (`xs[lo..hi]`).
//! - The checks that need the element type resolved: what `contains`, `sort`,
//!   `sum` and friends need of it, and that `enumerate()` is only a `for`
//!   loop's iterable.

use la_arena::ArenaMap;

use super::{error_type, Expectation, InferTy, InferenceDiagnostic, InferenceResultBuilder};
use crate::{
    builtin::{ArrayMethod, ElementRequirement},
    expr::{Expr, ExprId},
    ty::{primitives::IntTy, Ty, TyKind},
    BuiltinFn, HirDisplay,
};

impl InferenceResultBuilder<'_> {
    /// Infers `receiver.method(args)` for an array `receiver` whose element
    /// type is `elem`.
    pub(super) fn infer_array_method(
        &mut self,
        tgt_expr: ExprId,
        method: ArrayMethod,
        elem: &Ty,
        args: &[ExprId],
    ) -> Ty {
        self.builtin_calls
            .insert(tgt_expr, BuiltinFn::Array(method));
        let usize_ty = || TyKind::Int(IntTy::usize()).intern();
        let unit = Ty::unit;
        let optional = |ty: Ty| TyKind::Optional(ty).intern();
        let array = |ty: Ty| TyKind::Array(ty).intern();
        let arity = match method {
            ArrayMethod::Insert | ArrayMethod::Fold => 2,
            ArrayMethod::Push
            | ArrayMethod::Remove
            | ArrayMethod::Reserve
            | ArrayMethod::Append
            | ArrayMethod::Contains
            | ArrayMethod::IndexOf
            | ArrayMethod::BinarySearch
            | ArrayMethod::Map
            | ArrayMethod::Filter
            | ArrayMethod::Any
            | ArrayMethod::All
            | ArrayMethod::PushStr
            | ArrayMethod::PushInt
            | ArrayMethod::PushJsonFloat => 1,
            _ => 0,
        };
        if args.len() != arity {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: arity,
                });
            for &arg in args {
                self.infer_expr(arg, &Expectation::none());
            }
            return error_type();
        }
        let elem = elem.clone();
        match method {
            ArrayMethod::Push => {
                self.infer_expr_coerce(args[0], &Expectation::has_type(elem));
                unit()
            }
            ArrayMethod::Pop | ArrayMethod::Last | ArrayMethod::Min | ArrayMethod::Max => {
                optional(elem)
            }
            ArrayMethod::Insert => {
                self.infer_index_argument(args[0]);
                self.infer_expr_coerce(args[1], &Expectation::has_type(elem));
                unit()
            }
            ArrayMethod::Remove => {
                self.infer_index_argument(args[0]);
                elem
            }
            ArrayMethod::Reserve => {
                self.infer_index_argument(args[0]);
                unit()
            }
            ArrayMethod::Clear | ArrayMethod::Reverse | ArrayMethod::Sort => unit(),
            ArrayMethod::Capacity => usize_ty(),
            ArrayMethod::IsEmpty => TyKind::Bool.intern(),
            ArrayMethod::Append => {
                self.infer_expr(args[0], &Expectation::has_type(array(elem)));
                unit()
            }
            ArrayMethod::Contains => {
                self.infer_expr_coerce(args[0], &Expectation::has_type(elem));
                TyKind::Bool.intern()
            }
            ArrayMethod::IndexOf | ArrayMethod::BinarySearch => {
                self.infer_expr_coerce(args[0], &Expectation::has_type(elem));
                optional(usize_ty())
            }
            ArrayMethod::Copy => array(elem),
            ArrayMethod::Sum => elem,
            ArrayMethod::Map => {
                let result = self.type_variables.new_type_var();
                let f = Ty::function(vec![elem], result.clone());
                let f = self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                let result = f
                    .as_function()
                    .map(|(_, ret)| ret.clone())
                    .unwrap_or(result);
                array(self.resolve_ty_as_far_as_possible(result))
            }
            ArrayMethod::Filter => {
                let f = Ty::function(vec![elem.clone()], TyKind::Bool.intern());
                self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                array(elem)
            }
            ArrayMethod::Any | ArrayMethod::All => {
                let f = Ty::function(vec![elem], TyKind::Bool.intern());
                self.infer_expr_coerce(args[0], &Expectation::has_type(f));
                TyKind::Bool.intern()
            }
            ArrayMethod::Fold => {
                let acc = self.infer_expr(args[0], &Expectation::none());
                let f = Ty::function(vec![acc.clone(), elem], acc.clone());
                self.infer_expr_coerce(args[1], &Expectation::has_type(f));
                self.resolve_ty_as_far_as_possible(acc)
            }
            ArrayMethod::PushStr => {
                let ty = self.infer_expr(args[0], &Expectation::none());
                let ty = self.normalize(&ty);
                let silent = matches!(
                    ty.interned(),
                    TyKind::Unknown | TyKind::Never | TyKind::InferenceVar(InferTy::Type(_))
                );
                if !matches!(ty.interned(), TyKind::String | TyKind::Str) && !silent {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: args[0].into(),
                        message: format!(
                            "`push_str` takes a `String` or a `str`, not `{}`",
                            ty.display(self.db)
                        ),
                    });
                }
                unit()
            }
            ArrayMethod::PushInt => {
                let ty = self.type_variables.new_integer_var();
                let ty = self.infer_expr(args[0], &Expectation::has_type(ty));
                // A bare literal is an `i64` here, not the `i32` an
                // unconstrained integer defaults to: its text must not wrap.
                let ty = self.normalize(&ty);
                if matches!(ty.interned(), TyKind::InferenceVar(InferTy::Int(_))) {
                    self.unify(&ty, &TyKind::Int(IntTy::i64()).intern());
                }
                unit()
            }
            ArrayMethod::PushJsonFloat => {
                let f64_ty = TyKind::Float(crate::FloatTy::f64()).intern();
                self.infer_expr_coerce(args[0], &Expectation::has_type(f64_ty));
                unit()
            }
            ArrayMethod::Enumerate => {
                let pair = TyKind::Tuple(2, [usize_ty(), elem].into_iter().collect()).intern();
                array(pair)
            }
        }
    }

    /// An index or count argument: any integer type. A negative value is
    /// out of range at run time.
    fn infer_index_argument(&mut self, arg: ExprId) {
        let index_ty = self.type_variables.new_integer_var();
        self.infer_expr(arg, &Expectation::has_type(index_ty));
    }

    /// Infers `base[lo..hi]`: a new array of the elements from `lo` up to,
    /// not including, `hi` (`..=` includes `hi`).
    pub(super) fn infer_array_slice(
        &mut self,
        base: ExprId,
        start: ExprId,
        end: ExprId,
        expected: &Expectation,
    ) -> Ty {
        let expected_ty = if expected.ty.is_unknown() {
            TyKind::Array(self.type_variables.new_type_var()).intern()
        } else {
            expected.ty.clone()
        };
        let base_ty = self.infer_expr(base, &Expectation::has_type(expected_ty));
        self.infer_index_argument(start);
        self.infer_index_argument(end);
        match base_ty.interned() {
            TyKind::Array(_) => base_ty,
            _ => error_type(),
        }
    }

    /// After inference: checks what the array methods need of their
    /// (now resolved) element types, and that `enumerate()` is only ever a
    /// `for` loop's iterable.
    pub(super) fn check_array_methods(&mut self, expr_types: &ArenaMap<ExprId, Ty>) {
        let mut calls: Vec<(ExprId, ArrayMethod)> = self
            .builtin_calls
            .iter()
            .filter_map(|(&expr, &builtin)| match builtin {
                BuiltinFn::Array(method) => Some((expr, method)),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return;
        }
        calls.sort_by_key(|(expr, _)| u32::from(expr.into_raw()));
        let for_iterables: Vec<ExprId> = self
            .body
            .exprs()
            .filter_map(|(_, expr)| match expr {
                Expr::For { iterable, .. } => Some(*iterable),
                _ => None,
            })
            .collect();
        for (expr, method) in calls {
            let Expr::MethodCall { receiver, args, .. } = &self.body[expr] else {
                continue;
            };
            if method == ArrayMethod::PushInt {
                let wide = args.first().and_then(|arg| expr_types.get(*arg)).is_some_and(
                    |ty| matches!(ty.interned(), TyKind::Int(int) if int.bitness == crate::primitive_type::IntBitness::X128),
                );
                if wide {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: expr.into(),
                        message: "`push_int` takes an integer of at most 64 bits; write \
                                  `bytes.push_str(n.to_string())` for a 128-bit one"
                            .to_string(),
                    });
                }
            }
            if method == ArrayMethod::Enumerate {
                if !for_iterables.contains(&expr) {
                    self.diagnostics.push(InferenceDiagnostic::ExprError {
                        id: expr.into(),
                        message: "`enumerate()` can only be the iterable of a `for` loop \
                                  (`for (i, x) in xs.enumerate() { .. }`)"
                            .to_string(),
                    });
                }
                continue;
            }
            let Some(requirement) = method.element_requirement() else {
                continue;
            };
            let Some(elem) = expr_types.get(*receiver).and_then(|ty| ty.as_array()) else {
                continue;
            };
            if elem.is_unknown() {
                continue;
            }
            if element_meets(requirement, elem) {
                continue;
            }
            let what = requirement_text(requirement);
            let hint = match elem.interned() {
                _ if requirement == ElementRequirement::Bytes => "",
                TyKind::TypeParam(..) => {
                    "; inside a generic function the element type is not known yet, so \
                     write the loop out and call a trait method of a bound"
                }
                _ => {
                    "; for other types, write the loop out (or use `fold`/`any` with a \
                      closure)"
                }
            };
            self.diagnostics.push(InferenceDiagnostic::ExprError {
                id: expr.into(),
                message: format!(
                    "`{}` needs an array of {what}, not of `{}`{hint}",
                    method.name(),
                    elem.display(self.db)
                ),
            });
        }
    }
}

/// Whether elements of type `elem` support what `requirement` asks for.
pub(super) fn element_meets(requirement: ElementRequirement, elem: &Ty) -> bool {
    match (requirement, elem.interned()) {
        (ElementRequirement::Bytes, TyKind::Int(int)) => *int == IntTy::u8(),
        (ElementRequirement::Bytes, TyKind::InferenceVar(InferTy::Int(_))) => true,
        (ElementRequirement::Bytes, _) => false,
        (_, TyKind::Int(_) | TyKind::Float(_)) => true,
        (_, TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_))) => true,
        (ElementRequirement::Number, _) => false,
        (_, TyKind::Bool) => true,
        (
            ElementRequirement::Equality | ElementRequirement::Order { str_allowed: true },
            TyKind::Str,
        ) => true,
        _ => false,
    }
}

/// The element types `requirement` allows, for a diagnostic.
pub(super) fn requirement_text(requirement: ElementRequirement) -> &'static str {
    match requirement {
        ElementRequirement::Equality | ElementRequirement::Order { str_allowed: true } => {
            "integers, floats, `bool` or `str`"
        }
        ElementRequirement::Order { str_allowed: false } => "integers, floats or `bool`",
        ElementRequirement::Number => "integers or floats",
        ElementRequirement::Bytes => "`u8` (a byte buffer)",
    }
}
