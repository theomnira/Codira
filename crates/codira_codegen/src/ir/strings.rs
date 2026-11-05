//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - What the collection passes need to know about `String`: which
//!   expressions call the runtime's string functions, and which array types
//!   those functions allocate. The lowering itself is in `ir::body::strings`.

use codira_hir::{BinaryOp, Body, BuiltinFn, Expr, ExprId, InferenceResult, IntTy, Ty, TyKind};

/// Whether `expr` is lowered to calls of the runtime's string functions: a
/// `String`-typed expression, a builtin string operation, or a string
/// operator (including `==` and `<` on two `str`s).
pub(crate) fn uses_strings(body: &Body, infer: &InferenceResult, expr: ExprId) -> bool {
    if needs_string_types(infer, expr) {
        return true;
    }
    match &body[expr] {
        Expr::BinaryOp {
            lhs,
            op: Some(op),
            ..
        } => {
            !matches!(op, BinaryOp::Assignment { op: None } | BinaryOp::LogicOp(_))
                && matches!(infer[*lhs].interned(), TyKind::String | TyKind::Str)
        }
        _ => false,
    }
}

/// Whether `expr` may allocate a string buffer or an array of strings or
/// code points, and so needs [`runtime_types`] in the type table.
pub(crate) fn needs_string_types(infer: &InferenceResult, expr: ExprId) -> bool {
    matches!(infer[expr].interned(), TyKind::String)
        || matches!(infer.builtin_call(expr), Some(BuiltinFn::String(_)))
        || infer.is_string_conversion(expr)
}

/// The types the runtime's string functions are handed handles of: the
/// `[u8]` buffer, `[String]` (`split`, `lines`) and `[u32]` (`chars`).
pub(crate) fn runtime_types() -> [Ty; 3] {
    [byte_array(), string_array(), code_point_array()]
}

/// `[String]`.
pub(crate) fn string_array() -> Ty {
    TyKind::Array(TyKind::String.intern()).intern()
}

/// `[u32]`, what `chars()` returns.
pub(crate) fn code_point_array() -> Ty {
    TyKind::Array(TyKind::Int(IntTy::u32()).intern()).intern()
}

/// `[u8]`, the type of a string's buffer.
pub(crate) fn byte_array() -> Ty {
    TyKind::Array(TyKind::Int(IntTy::u8()).intern()).intern()
}
