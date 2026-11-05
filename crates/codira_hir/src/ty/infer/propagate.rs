//! Copyright (c) 2026 Omnira CJSC
//! Date: November 5, 2026
//!
//! Functionality:
//! - Type checking of postfix `?`, error propagation.
//!
//! `expr?` has two forms, chosen by the operand's type:
//!
//! - On a `Result[T, E]` it is the `T` inside `Ok`, or it returns `Err(e)`
//!   from the enclosing function (or closure), whose result type must be a
//!   `Result[_, E]` of the same `Result` declaration with the same `E`.
//!   There is no conversion between error types: a different `E` is an
//!   error that names both.
//! - On a `T?` it is the `T`, or it returns `nil`; the enclosing result
//!   type must be an optional (of any payload).
//!
//! A *`Result`* is any enum declared as `Result` with exactly two variants,
//! `Ok` and `Err`, each with one payload -- `std.result.result`'s, or a
//! program's own with that shape. The rule is structural so that `?` does
//! not depend on which module declares the type, and the "same declaration"
//! requirement keeps two unrelated `Result`s from being confused.

use super::InferenceResultBuilder;
use crate::{
    code_model::{Enum, EnumVariant, VariantKind},
    expr::ExprId,
    ty::{infer::diagnostics::InferenceDiagnostic, Ty, TyKind},
    HirDatabase, HirDisplay,
};

/// The `Ok` and `Err` variants of `e`, when `e` is a `Result` in the sense
/// of the module documentation.
pub fn result_variants(db: &dyn HirDatabase, e: Enum) -> Option<(EnumVariant, EnumVariant)> {
    if e.name(db).as_str() != Some("Result") {
        return None;
    }
    let variants = e.variants(db);
    if variants.len() != 2 {
        return None;
    }
    let find = |name: &str| {
        variants.iter().copied().find(|v| {
            v.name(db).as_str() == Some(name)
                && v.kind(db) == VariantKind::Tuple
                && v.field_types(db).len() == 1
        })
    };
    Some((find("Ok")?, find("Err")?))
}

impl InferenceResultBuilder<'_> {
    /// Infers `operand?` (see the module documentation) and returns the
    /// type of its value: the `Ok` payload or the optional's payload.
    pub(super) fn infer_propagate(&mut self, tgt_expr: ExprId, operand: ExprId) -> Ty {
        let operand_ty = self.infer_expr(operand, &super::Expectation::none());
        let operand_ty = self.normalize(&operand_ty);
        let return_ty = self.return_ty.clone();
        let returned = self.normalize(&return_ty);
        match operand_ty.interned().clone() {
            TyKind::Unknown => super::error_type(),
            TyKind::Optional(payload) => {
                match returned.interned() {
                    TyKind::Optional(_) | TyKind::Unknown => {}
                    _ => self.propagate_error(
                        tgt_expr,
                        format!(
                            "`?` on an optional returns `nil` early, so the enclosing function \
                             must return an optional, but it returns `{}`; unwrap with `!`, or \
                             test with `if let`",
                            return_ty.display(self.db)
                        ),
                    ),
                }
                self.propagations.insert(tgt_expr, return_ty);
                payload
            }
            TyKind::Enum(e, args) => {
                let Some((ok, err)) = result_variants(self.db, e) else {
                    self.propagate_error(
                        tgt_expr,
                        format!(
                            "`?` applies to a `Result[T, E]` (an enum `Result` with variants \
                             `Ok(T)` and `Err(E)`) or an optional `T?`, but this is `{}`",
                            operand_ty.display(self.db)
                        ),
                    );
                    return super::error_type();
                };
                let ok_ty = ok.field_types_at(self.db, &args).remove(0);
                let err_ty = err.field_types_at(self.db, &args).remove(0);
                // A closure whose result type is not written yet gets one
                // here: `Result[_, E]`.
                if let TyKind::InferenceVar(_) = returned.interned() {
                    let fresh = self.instantiate_type(&e.ty(self.db));
                    self.unify(&fresh, &returned);
                }
                let returned = self.normalize(&return_ty);
                match returned.interned().clone() {
                    TyKind::Enum(r, ret_args) if r == e => {
                        let ret_err = err.field_types_at(self.db, &ret_args).remove(0);
                        if !self.unify(&ret_err, &err_ty) {
                            self.propagate_error(
                                tgt_expr,
                                format!(
                                    "`?` would return this `Err({})` from a function that \
                                     returns `{}`: the error types must be the same (there is \
                                     no conversion between them); `match` and build the error \
                                     the function returns",
                                    err_ty.display(self.db),
                                    returned.display(self.db)
                                ),
                            );
                        }
                    }
                    TyKind::Unknown => {}
                    _ => self.propagate_error(
                        tgt_expr,
                        format!(
                            "`?` on a `{}` returns its `Err` early, so the enclosing function \
                             must return a `Result[_, {}]`, but it returns `{}`",
                            operand_ty.display(self.db),
                            err_ty.display(self.db),
                            return_ty.display(self.db)
                        ),
                    ),
                }
                self.propagations.insert(tgt_expr, return_ty);
                ok_ty
            }
            _ => {
                self.propagate_error(
                    tgt_expr,
                    format!(
                        "`?` applies to a `Result[T, E]` or an optional `T?`, but this is `{}`",
                        operand_ty.display(self.db)
                    ),
                );
                super::error_type()
            }
        }
    }

    fn propagate_error(&mut self, id: ExprId, message: String) {
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: id.into(),
            message,
        });
    }
}
