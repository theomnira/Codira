//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Type checking of the holes with a format spec in an interpolated
//!   literal (`f"{x:>8.3}"`, `Expr::Format`; see `builtin::format`).
//!
//! The value is inferred like any expression, a `{w}` width or precision as
//! an integer of any type, and the hole is a `String`. Whether the spec
//! applies to the value's type is checked once every type is known
//! ([`InferenceResultBuilder::check_formats`]): an unsuffixed integer
//! literal's type, for one, is decided only at the end. The error is at the
//! literal and names the spec and the type.

use la_arena::ArenaMap;

use super::{Expectation, InferenceDiagnostic, InferenceResultBuilder};
use crate::{
    expr::{Expr, ExprId},
    ty::{Ty, TyKind},
    FormatSpec, HirDisplay, ValueClass,
};

/// The class of value `ty` is, for [`FormatSpec::check`]; `None` when it is
/// an error already reported.
fn value_class(ty: &Ty) -> Option<ValueClass> {
    Some(match ty.interned() {
        TyKind::Int(_) => ValueClass::Integer,
        TyKind::Float(_) => ValueClass::Float,
        TyKind::String | TyKind::Str => ValueClass::Text,
        TyKind::Bool => ValueClass::Bool,
        TyKind::TypeParam(..) => ValueClass::TypeParameter,
        TyKind::Unknown | TyKind::Never | TyKind::InferenceVar(_) => return None,
        _ => ValueClass::Other,
    })
}

impl InferenceResultBuilder<'_> {
    /// Infers `{value:spec}`, whose width and precision variables are
    /// `width` and `precision`.
    pub(super) fn infer_format(
        &mut self,
        value: ExprId,
        width: Option<ExprId>,
        precision: Option<ExprId>,
    ) -> Ty {
        self.infer_expr(value, &Expectation::none());
        for count in width.into_iter().chain(precision) {
            let integer = self.type_variables.new_integer_var();
            self.infer_expr(count, &Expectation::has_type(integer));
        }
        TyKind::String.intern()
    }

    /// Checks every hole's spec against its value's resolved type.
    pub(super) fn check_formats(&mut self, expr_types: &ArenaMap<ExprId, Ty>) {
        let holes: Vec<(ExprId, ExprId, FormatSpec)> = self
            .body
            .exprs()
            .filter_map(|(id, expr)| match expr {
                Expr::Format { value, spec, .. } => Some((id, *value, spec.clone())),
                _ => None,
            })
            .collect();
        for (hole, value, spec) in holes {
            let Some(ty) = expr_types.get(value) else {
                continue;
            };
            let Some(class) = value_class(ty) else {
                continue;
            };
            if let Err(reason) = spec.check(class) {
                self.diagnostics.push(InferenceDiagnostic::ExprError {
                    id: hole.into(),
                    message: format!(
                        "the format spec `{}` does not apply to a `{}`: {reason}",
                        spec.text,
                        ty.display(self.db)
                    ),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    #[test]
    fn specs_that_apply_type_check() {
        let diags = diagnostics(
            r#"
    struct P { x: i64, y: f64 }
    enum Shape { Circle(f64), Dot }
    struct Box[T] { v: T }
    func show[T](x: T) -> String { f"{x:?} {x:#?}" + x.debug_string() }
    func f(n: i64, u: u8, x: f64, s: String, t: str, b: bool, w: usize, p: i32) -> String {
        let k = 255
        let pt = P { x: 1, y: 2.0 }
        let shapes = [Shape.Circle(1.0), Shape.Dot]
        let o: i64? = nil
        let bx = Box { v: pt }
        let a = f"{n:>8} {n:<8} {n:^8} {n:*^9} {n:08} {n:+} {u:#x} {u:X} {k:#o} {k:b} {n:e}"
        let c = f"{x:.3} {x:>10.3} {x:+.0} {x:e} {x:.2E} {x:>{w}.{p}} {n:>{w}}"
        let d = f"{s:>5} {s:.2} {t:-^7.1} {b:>6} {pt:?} {shapes:#?} {o:?} {bx:>30?}"
        a + c + d + pt.debug_string() + show(bx) + show(3)
    }
    "#,
        );
        assert_eq!(diags, "", "{diags}");
    }

    #[test]
    fn a_spec_that_does_not_apply_is_an_error_naming_it_and_the_type() {
        let diags = diagnostics(
            r#"
    struct P { x: i64 }
    func g[T](x: T) -> String { f"{x:>4}" }
    func f(n: i64, x: f64, s: String, b: bool, pt: P, name: String) -> String {
        let a = f"{n:.2}"
        let c = f"{x:x}"
        let d = f"{s:+}"
        let e = f"{pt:>4}"
        let h = f"{b:.1}"
        let i = f"{s:08}"
        let j = f"{n:#}"
        let k = f"{x:.2?}"
        let l = f"{n:>{name}}"
        a + c + d + e + h + i + j + k + l
    }
    "#,
        );
        for needle in [
            "the format spec `.2` does not apply to a `i64`: integers have no precision",
            "the format spec `x` does not apply to a `f64`: `x` (a radix) applies to integers only",
            "the format spec `+` does not apply to a `String`: `+` applies to numbers only",
            "the format spec `>4` does not apply to a `P`: it has no `to_string()`",
            "the format spec `.1` does not apply to a `bool`",
            "the format spec `08` does not apply to a `String`: `0` applies to numbers only",
            "the format spec `#` does not apply to a `i64`: `#` applies to the radix forms",
            "the format spec `.2?` does not apply to a `f64`: the debug form `?` takes",
            "the format spec `>4` does not apply to a `T`: a value of a generic type takes only \
             the debug form",
            "mismatched type",
        ] {
            assert!(diags.contains(needle), "missing `{needle}` in:\n{diags}");
        }
    }

    #[test]
    fn a_malformed_spec_is_an_error_at_the_literal() {
        let diags = diagnostics(
            r#"
    func f(n: i64) -> String {
        let a = f"{n:q}"
        let b = f"{n:8.}"
        let c = f"{n:>{1}}"
        a + b + c
    }
    "#,
        );
        for needle in [
            "invalid format spec: `q`: unknown format type `q`",
            "invalid format spec: `8.`: a `.` must be followed by a precision",
            "invalid format spec: `>{1}`: a width in braces names a variable",
        ] {
            assert!(diags.contains(needle), "missing `{needle}` in:\n{diags}");
        }
    }
}
