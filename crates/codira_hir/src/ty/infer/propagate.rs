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
//! - Otherwise, when `F` declares an *error conversion* -- an associated
//!   function marked `@error_conversion` that takes one parameter and
//!   returns an `F` -- and an `E` can be passed to it, the error leaves as
//!   `F.conversion(e)`. `std.error.error`'s `AnyError` declares
//!   `from_error[E: Error](error: E) -> AnyError`, so in a function returning
//!   `Result[_, AnyError]`, `?` passes on an error of any type implementing
//!   `Error`. The conversion is an ordinary generic call, instantiated per
//!   error type and resolved statically; a type the conversion's bounds
//!   rule out is an error at the `?` naming the bound.
//! - On a `T?` it is the `T`, or it returns `nil`; the enclosing result
//!   type must be an optional (of any payload).
//!
//! The conversion is found by its attribute rather than by its name or by
//! where it is declared, so the compiler knows no standard-library path:
//! any type, a program's own included, can declare one. The rules apply in
//! order -- the same type, then a wrapping variant, then a conversion -- so
//! `?` between two `Result[_, AnyError]`s passes the error on as it is.
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
    ids::AssocItemId,
    method_resolution::{AssociationMode, MethodResolutionCtx},
    ty::{infer::diagnostics::InferenceDiagnostic, Ty, TyKind},
    Function, HirDatabase, HirDisplay,
};

/// The `@error_conversion` functions of `ty`: its associated functions (no
/// `self`) that carry the attribute.
pub fn error_conversions(db: &dyn HirDatabase, ty: &Ty) -> Vec<Function> {
    let mut found = Vec::new();
    MethodResolutionCtx::new(db, ty.clone())
        .with_association(AssociationMode::WithoutSelf)
        .collect(|item, _visible| -> Option<()> {
            let AssocItemId::FunctionId(f) = item;
            let f = Function::from(f);
            if f.is_error_conversion(db) {
                found.push(f);
            }
            None
        });
    found
}

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

/// What a `main` that returns a `Result` does with an `Err`: see
/// [`main_result`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainResult {
    /// The `Ok` variant of `main`'s result type.
    pub ok: EnumVariant,
    /// The `Ok` payload's type: what `main` returns to the host.
    pub ok_ty: Ty,
    /// The `Err` variant of `main`'s result type.
    pub err: EnumVariant,
    /// The error type.
    pub error_ty: Ty,
    /// The error type's own `to_string(self) -> String`, if it declares one:
    /// what the error is written as (`AnyError`'s writes its chain).
    pub to_string: Option<Function>,
}

/// For `main` in the root module of a package, returning a `Result[T, E]`:
/// how its `Err` is reported. The program then ends with exit status 1
/// after writing `Error: ` and the error to standard error -- the error's
/// `to_string()` when its type declares one or has one built in (a number,
/// a `bool`, a string), its debug form otherwise -- as Rust's `main`
/// reports one with `Debug`. To the host, `main` returns the `Ok` payload:
/// its exported signature is `main() -> T`, so `codira run` prints a
/// primitive `T` as it would for a `main` returning one. `None` for any
/// other function.
pub fn main_result(db: &dyn HirDatabase, function: Function) -> Option<MainResult> {
    if function.name(db).as_str() != Some("main") || function.module(db).parent(db).is_some() {
        return None;
    }
    if function.self_param_ty(db).is_some() || function.is_generic(db) {
        return None;
    }
    let ret = function.ret_type(db);
    let TyKind::Enum(e, args) = ret.interned() else {
        return None;
    };
    let (ok, err) = result_variants(db, *e)?;
    let ok_ty = ok.field_types_at(db, args).remove(0);
    let error_ty = err.field_types_at(db, args).remove(0);
    let to_string = match error_ty.interned() {
        TyKind::Struct(..) | TyKind::Enum(..) => MethodResolutionCtx::new(db, error_ty.clone())
            .with_association(AssociationMode::WithSelf)
            .with_name(crate::Name::new("to_string"))
            .collect(|item, _visible| {
                let AssocItemId::FunctionId(f) = item;
                let f = Function::from(f);
                let sig = db.callable_sig(f.into());
                (sig.params().is_empty() && sig.ret().is_string()).then_some(f)
            }),
        _ => None,
    };
    Some(MainResult {
        ok,
        ok_ty,
        err,
        error_ty,
        to_string,
    })
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
                            Ok(None) if self.error_conversion(tgt_expr, &ret_err, &err_ty) => {}
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

    /// Passes an error of type `inner` on as an `outer` through `outer`'s
    /// `@error_conversion`, if it has one: records the call (its function and
    /// instantiation, which code generation and monomorphisation read like
    /// those of a call expression) and returns `true`. A conversion that
    /// cannot take an `inner` is reported here, and also returns `true`: the
    /// error is about the conversion, not about the types being different.
    fn error_conversion(&mut self, id: ExprId, outer: &Ty, inner: &Ty) -> bool {
        let outer = self.normalize(outer);
        let inner = self.normalize(inner);
        if matches!(inner.interned(), TyKind::InferenceVar(_) | TyKind::Unknown) {
            return false;
        }
        let conversions = error_conversions(self.db, &outer);
        let conversion = match conversions.as_slice() {
            [] => return false,
            [conversion] => *conversion,
            several => {
                let names: Vec<String> = several
                    .iter()
                    .map(|f| format!("`{}`", f.name(self.db)))
                    .collect();
                self.propagate_error(
                    id,
                    format!(
                        "`?` cannot tell which `@error_conversion` of `{}` to pass this `{}` \
                         error through: {} are all marked",
                        outer.display(self.db),
                        inner.display(self.db),
                        names.join(", ")
                    ),
                );
                return true;
            }
        };
        let name = format!("{}.{}", outer.display(self.db), conversion.name(self.db));
        let signature = self.db.callable_sig(conversion.into());
        if signature.params().len() != 1 {
            self.propagate_error(
                id,
                format!(
                    "`{name}` is marked `@error_conversion`, so it must take one parameter (the \
                     error) and return a `{}`, but it takes {}",
                    outer.display(self.db),
                    signature.params().len()
                ),
            );
            return true;
        }
        let (params, ret, fresh) =
            self.instantiate_signature_with_args(signature.params(), signature.ret());
        if !self.unify(&ret, &outer) {
            self.propagate_error(
                id,
                format!(
                    "`{name}` is marked `@error_conversion`, so it must return a `{}`, but it \
                     returns `{}`",
                    outer.display(self.db),
                    signature.ret().display(self.db)
                ),
            );
            return true;
        }
        if !self.unify(&params[0], &inner) {
            self.propagate_error(
                id,
                format!(
                    "`?` would pass this `{}` error on as a `{}` through `{name}`, which takes a \
                     `{}`",
                    inner.display(self.db),
                    outer.display(self.db),
                    signature.params()[0].display(self.db)
                ),
            );
            return true;
        }
        // The bounds are checked here, so that the message names `?` and the
        // conversion rather than a call the program does not contain.
        let bounds = self.db.function_bounds(conversion.id);
        for bound in &bounds.bounds {
            let Some(arg) = fresh.get(&bound.param) else {
                continue;
            };
            let arg = self.normalize(arg);
            if matches!(arg.interned(), TyKind::InferenceVar(_))
                || crate::traits::implements(self.db, &arg, bound.trait_, &self.bounds)
            {
                continue;
            }
            let trait_name = bound.trait_.name(self.db);
            self.propagate_error(
                id,
                format!(
                    "`?` cannot pass this `{arg}` error on as a `{outer}`: `{name}` takes errors \
                     whose type implements `{trait_name}`, and `{arg}` does not; implement it \
                     with `extend {arg}: {trait_name} {{ .. }}`",
                    arg = arg.display(self.db),
                    outer = outer.display(self.db),
                ),
            );
            return true;
        }
        self.record_instantiation(id, conversion, &fresh);
        self.method_resolution.insert(id, conversion.id);
        true
    }

    fn propagate_error(&mut self, id: ExprId, message: String) {
        self.diagnostics.push(InferenceDiagnostic::ExprError {
            id: id.into(),
            message,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    /// A program's own error trait and any-error type, so the rule is seen
    /// to depend on the attribute and nothing in the standard library.
    const PRELUDE: &str = r#"
    enum Result[T, E] { Ok(T), Err(E) }
    trait Error { func message(self) -> String; }
    struct Any { message: String }
    extend Any {
        @error_conversion
        func from_error[E: Error](error: E) -> Any { Any { message: error.message() } }
    }
    struct Io { code: i64 }
    extend Io: Error { func message(self) -> String { f"io {self.code}" } }
    struct Plain { code: i64 }
    func io() -> Result[i64, Io] { Result.Err(Io { code: 2 }) }
    func plain() -> Result[i64, Plain] { Result.Err(Plain { code: 3 }) }
    func any() -> Result[i64, Any] { Result.Err(Any { message: "x" }) }
    "#;

    #[test]
    fn question_mark_converts_through_the_error_conversion() {
        let diags = diagnostics(&format!(
            "{PRELUDE}
    func converts() -> Result[i64, Any] {{ Result.Ok(io()? + any()?) }}
    func closure() -> i64 {{
        let f = func() -> Result[i64, Any] {{ Result.Ok(io()?) }};
        0
    }}
    "
        ));
        assert_eq!(diags, "", "{diags}");
    }

    #[test]
    fn a_conversion_that_does_not_apply_says_why() {
        let diags = diagnostics(&format!(
            "{PRELUDE}
    func not_error() -> Result[i64, Any] {{ Result.Ok(plain()?) }}
    struct Two {{ n: i64 }}
    extend Two {{
        @error_conversion
        func a[E: Error](error: E) -> Two {{ Two {{ n: 1 }} }}
        @error_conversion
        func b[E: Error](error: E) -> Two {{ Two {{ n: 2 }} }}
    }}
    func two() -> Result[i64, Two] {{ Result.Ok(io()?) }}
    struct Bad {{ n: i64 }}
    extend Bad {{
        @error_conversion
        func from(a: i64, b: i64) -> Bad {{ Bad {{ n: a + b }} }}
    }}
    func bad() -> Result[i64, Bad] {{ Result.Ok(io()?) }}
    struct Narrow {{ n: i64 }}
    extend Narrow {{
        @error_conversion
        func from(error: Plain) -> Narrow {{ Narrow {{ n: error.code }} }}
    }}
    func narrow() -> Result[i64, Narrow] {{ Result.Ok(io()?) }}
    "
        ));
        for needle in [
            "`?` cannot pass this `Plain` error on as a `Any`: `Any.from_error` takes errors \
             whose type implements `Error`, and `Plain` does not; implement it with `extend \
             Plain: Error { .. }`",
            "`?` cannot tell which `@error_conversion` of `Two` to pass this `Io` error through: \
             `a`, `b` are all marked",
            "`Bad.from` is marked `@error_conversion`, so it must take one parameter (the error) \
             and return a `Bad`, but it takes 2",
            "`?` would pass this `Io` error on as a `Narrow` through `Narrow.from`, which takes a \
             `Plain`",
        ] {
            assert!(diags.contains(needle), "missing `{needle}` in:\n{diags}");
        }
        // Each `?` is reported once, by the rule it breaks.
        assert_eq!(diags.lines().count(), 4, "{diags}");
    }
}
