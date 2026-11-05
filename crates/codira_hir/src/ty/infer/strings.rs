//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Type inference for `String`: its builtin methods (see
//!   [`crate::StringOp`]), `string_from_utf8`, and the operators.
//!
//! * A method's text arguments take a `String` or a `str`, so a literal is
//!   passed as it is: `s.find("x")` allocates nothing.
//! * `a + b` is a new `String` when each side is a `String` or a `str`
//!   (`"a" + "b"` included).
//! * `s += t` appends to a `String` place; `t` is a `String` or a `str`.
//! * `==`, `!=`, `<`, `<=`, `>`, `>=` compare two `String`s or `str`s, in any
//!   mix, by their bytes (which for UTF-8 is code-point order).
use super::{Expectation, InferenceDiagnostic, InferenceResultBuilder};
use crate::{
    expr::{ArithOp, BinaryOp, ExprId},
    ty::{infer::InferTy, IntTy, Ty, TyKind},
    BuiltinFn, HirDisplay, StringArg, StringOp,
};

/// Whether `ty` is a `String` or a `str`: what a text argument accepts.
fn is_text(ty: &Ty) -> bool {
    matches!(ty.interned(), TyKind::String | TyKind::Str)
}

/// Whether `ty` should not be reported again: an error already reported, or
/// a diverging expression.
fn is_silent(ty: &Ty) -> bool {
    matches!(
        ty.interned(),
        TyKind::Unknown | TyKind::Never | TyKind::InferenceVar(InferTy::Type(_))
    )
}

/// The builtin string method `name` of `receiver`, if there is one. An
/// unsuffixed literal's type is still an inference variable here; it has
/// `to_string()` like the integer or float it will default to.
pub(super) fn string_method(receiver: &Ty, name: &str) -> Option<StringOp> {
    match receiver.interned() {
        TyKind::InferenceVar(InferTy::Int(_) | InferTy::Float(_)) if name == "to_string" => {
            Some(StringOp::ToString)
        }
        kind => StringOp::method(kind, name),
    }
}

impl InferenceResultBuilder<'_> {
    /// Infers a builtin string method, `op`, called on `receiver_ty`.
    pub(super) fn infer_string_method(
        &mut self,
        tgt_expr: ExprId,
        op: StringOp,
        _receiver_ty: &Ty,
        args: &[ExprId],
    ) -> Ty {
        self.builtin_calls.insert(tgt_expr, BuiltinFn::String(op));
        self.infer_string_args(tgt_expr, op, args);
        op.result()
    }

    /// Infers `string_from_utf8(bytes)`.
    pub(super) fn infer_string_free_fn(
        &mut self,
        tgt_expr: ExprId,
        op: StringOp,
        args: &[ExprId],
    ) -> Ty {
        self.infer_string_args(tgt_expr, op, args);
        op.result()
    }

    fn infer_string_args(&mut self, tgt_expr: ExprId, op: StringOp, args: &[ExprId]) {
        let expected = op.args();
        if args.len() != expected.len() {
            self.diagnostics
                .push(InferenceDiagnostic::ParameterCountMismatch {
                    id: tgt_expr,
                    found: args.len(),
                    expected: expected.len(),
                });
        }
        for (index, &arg) in args.iter().enumerate() {
            match expected.get(index) {
                Some(StringArg::Text) => {
                    let ty = self.infer_expr(arg, &Expectation::none());
                    let ty = self.normalize(&ty);
                    if !is_text(&ty) && !is_silent(&ty) {
                        self.diagnostics.push(InferenceDiagnostic::ExprError {
                            id: arg.into(),
                            message: format!(
                                "`{}` takes a `String` or a `str`, not `{}`",
                                op.name(),
                                ty.display(self.db)
                            ),
                        });
                    }
                }
                Some(StringArg::Integer) => {
                    let index_ty = self.type_variables.new_integer_var();
                    self.infer_expr(arg, &Expectation::has_type(index_ty));
                }
                Some(StringArg::Bytes) => {
                    let bytes = TyKind::Array(TyKind::Int(IntTy::u8()).intern()).intern();
                    self.infer_expr(arg, &Expectation::has_type(bytes));
                }
                None => {
                    self.infer_expr(arg, &Expectation::none());
                }
            }
        }
    }

    /// The implicit `str` to `String` conversion: a `str` used where a
    /// `String` is expected (an argument, a `let` with a type, a field, a
    /// return value, an assignment to a `String`) becomes a `String` copy of
    /// its text. Records `expr` for code generation and returns `String`,
    /// or `None` when no conversion applies.
    pub(super) fn try_convert_to_string(&mut self, expr: ExprId, ty: &Ty, expected: &Ty) -> Option<Ty> {
        let expected = self.normalize(expected);
        let actual = self.normalize(ty);
        if !(expected.is_string() && matches!(actual.interned(), TyKind::Str)) {
            return None;
        }
        self.string_conversions.insert(expr);
        Some(expected)
    }

    /// Infers a binary operator whose left operand has type `lhs_ty`, if it
    /// is a string operator; `None` hands the expression back to the
    /// ordinary operator rules.
    pub(super) fn infer_string_binary_op(
        &mut self,
        tgt_expr: ExprId,
        op: BinaryOp,
        rhs: ExprId,
        lhs_ty: &Ty,
    ) -> Option<Ty> {
        let lhs_ty = self.normalize(lhs_ty);
        let result = match (op, lhs_ty.interned()) {
            (BinaryOp::ArithOp(ArithOp::Add), TyKind::String | TyKind::Str) => {
                TyKind::String.intern()
            }
            (BinaryOp::Assignment { op: Some(ArithOp::Add) }, TyKind::String) => Ty::unit(),
            (BinaryOp::CmpOp(_), TyKind::String) => TyKind::Bool.intern(),
            // `str` against `str` compares too; a `str` on the left of any
            // other operator keeps the existing rules (and their errors).
            (BinaryOp::CmpOp(_), TyKind::Str) => TyKind::Bool.intern(),
            _ => return None,
        };
        let rhs_ty = self.infer_expr(rhs, &Expectation::none());
        let rhs_ty = self.normalize(&rhs_ty);
        if !is_text(&rhs_ty) && !is_silent(&rhs_ty) {
            self.diagnostics
                .push(InferenceDiagnostic::CannotApplyBinaryOp {
                    id: tgt_expr,
                    lhs: lhs_ty,
                    rhs: rhs_ty,
                });
        }
        Some(result)
    }
}
