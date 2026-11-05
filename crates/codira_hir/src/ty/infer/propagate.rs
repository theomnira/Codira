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
//!   `Result[_, F]` of the same `Result` declaration. Either `F` is `E`, or
//!   `F` is an enum with exactly one variant whose one payload is an `E`,
//!   and the error is wrapped in it on the way out: with
//!   `enum Problem { Io(FileError), Parse(ParseError) }`, a `FileError`
//!   leaves a `-> Result[_, Problem]` function as `Problem.Io(e)`. This
//!   is what Rust's `From` implementations are written for, read off the
//!   declaration instead; it is decided statically and is never ambiguous
//!   (two variants holding an `E` are an error).
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
                        match self.error_wrap(&ret_err, &err_ty) {
                            Ok(Some(variant)) => {
                                self.propagation_wraps.insert(tgt_expr, variant);
                            }
                            Ok(None) if self.unify(&ret_err, &err_ty) => {}
                            Ok(None) => {
                                let err = err_ty.display(self.db).to_string();
                                let wrap = if self.normalize(&ret_err).as_enum().is_some() {
                                    format!(
                                        ", or `{}` must have exactly one variant holding a \
                                         `{err}`, which `?` wraps the error in",
                                        ret_err.display(self.db)
                                    )
                                } else {
                                    String::new()
                                };
                                self.propagate_error(
                                    tgt_expr,
                                    format!(
                                        "`?` would return this `Err({err})` from a function that \
                                         returns `{}`: the error types must be the same{wrap}",
                                        returned.display(self.db),
                                    ),
                                );
                            }
                            Err(variants) => self.propagate_error(
                                tgt_expr,
                                format!(
                                    "`?` cannot tell which variant of `{}` to wrap this `{}` \
                                     error in: {} all hold one; `match` and build the error",
                                    ret_err.display(self.db),
                                    err_ty.display(self.db),
                                    variants.join(", "),
                                ),
                            ),
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

    /// The variant of `outer` that wraps an error of type `inner` when `?`
    /// passes it on (see the module documentation): `Ok(None)` when no
    /// wrapping applies (the types are meant to be the same), `Err` with the
    /// candidates when more than one variant could.
    fn error_wrap(&mut self, outer: &Ty, inner: &Ty) -> Result<Option<EnumVariant>, Vec<String>> {
        let outer = self.normalize(outer);
        let inner = self.normalize(inner);
        let TyKind::Enum(wrapper, args) = outer.interned() else {
            return Ok(None);
        };
        if matches!(inner.interned(), TyKind::Enum(e, _) if e == wrapper)
            || matches!(inner.interned(), TyKind::InferenceVar(_))
        {
            return Ok(None);
        }
        let mut found = Vec::new();
        for variant in wrapper.variants(self.db) {
            if let [payload] = variant.field_types_at(self.db, args).as_slice() {
                if self.normalize(payload) == inner {
                    found.push(variant);
                }
            }
        }
        match found.as_slice() {
            [] => Ok(None),
            [variant] => Ok(Some(*variant)),
            _ => Err(found
                .iter()
                .map(|v| format!("`{}`", v.display(self.db)))
                .collect()),
        }
    }

    fn propagate_error(&mut self, id: ExprId, message: String) {
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: id.into(),
            message,
        });
    }
}
