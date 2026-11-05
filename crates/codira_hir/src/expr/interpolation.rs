//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Interpolated string literals: `f"x = {x}, p.y = {p.y}"` is a `String`,
//!   lowered here to `"x = ".to_string() + x.to_string() + ", p.y = " +
//!   p.y.to_string()`.
//!
//! A hole names a local or a parameter, optionally followed by fields
//! (`{p.pos.x}`, `{self.name}`), as Rust's `format!("{x}")` names a variable:
//! no arbitrary expressions, so a hole never hides a call or a side effect.
//! Each hole is converted with `to_string()`, so it type-checks like that
//! call would: a hole whose type has no `to_string` (an array, a struct) is
//! an error at the literal. `{{` and `}}` are a literal brace. The pieces
//! are appended to one `String`, which ends at its buffer's fill after the
//! first append, so the appends after it extend that buffer in place
//! (doubling it when full) rather than copying the text so far.

use codira_syntax::{ast, AstPtr};

use super::{ArithOp, BinaryOp, Expr, ExprCollector, ExprId, Literal, LiteralError};
use crate::{
    name::Name,
    path::{Path, PathKind},
};

/// One piece of an interpolated literal.
enum Piece {
    /// Text, still escaped as written.
    Text(String),
    /// The dotted name in a hole: a variable, then fields.
    Hole(Vec<String>),
}

/// Splits the inside of `f"..."` into pieces, or `None` when a brace is
/// unbalanced or a hole is not a dotted name.
fn pieces(inner: &str) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // An escape is kept for `string_lit`; the escaped character
                // is never a hole's brace.
                text.push(c);
                if let Some(next) = chars.next() {
                    text.push(next);
                }
            }
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                let mut hole = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        c => hole.push(c),
                    }
                }
                let parts: Vec<String> = hole.trim().split('.').map(str::to_owned).collect();
                let is_name = |part: &String| {
                    let mut chars = part.chars();
                    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                        && chars.all(|c| c.is_alphanumeric() || c == '_')
                };
                if !parts.iter().all(is_name) {
                    return None;
                }
                pieces.push(Piece::Text(std::mem::take(&mut text)));
                pieces.push(Piece::Hole(parts));
            }
            '}' => return None,
            c => text.push(c),
        }
    }
    pieces.push(Piece::Text(text));
    Some(pieces)
}

impl ExprCollector<'_> {
    /// Lowers the interpolated literal `text` (`f"..."`, as written).
    pub(super) fn collect_interpolated(&mut self, text: &str, ptr: AstPtr<ast::Expr>) -> ExprId {
        let inner = text
            .strip_prefix("f\"")
            .map(|rest| rest.strip_suffix('"').unwrap_or(rest))
            .unwrap_or(text);
        let Some(pieces) = pieces(inner) else {
            let expr = self.alloc_expr(Expr::Literal(Literal::String(String::new())), ptr);
            self.diagnostics.push(super::ExprDiagnostic::LiteralError {
                expr,
                err: LiteralError::InvalidInterpolation,
            });
            return expr;
        };

        let to_string = |this: &mut Self, receiver: ExprId| {
            this.alloc_expr(
                Expr::MethodCall {
                    receiver,
                    method_name: Name::new("to_string"),
                    args: Vec::new(),
                },
                ptr.clone(),
            )
        };
        let mut result: Option<ExprId> = None;
        for piece in pieces {
            let (operand, is_text) = match piece {
                Piece::Text(escaped) => {
                    if escaped.is_empty() {
                        continue;
                    }
                    let (value, errors) = super::string_lit(&escaped);
                    let literal =
                        self.alloc_expr(Expr::Literal(Literal::String(value)), ptr.clone());
                    for err in errors {
                        self.diagnostics
                            .push(super::ExprDiagnostic::LiteralError { expr: literal, err });
                    }
                    (literal, true)
                }
                Piece::Hole(parts) => {
                    let mut parts = parts.into_iter();
                    let first = parts.next().expect("a hole names something");
                    let path = if first == "self" {
                        Path {
                            kind: PathKind::Super(0),
                            segments: Vec::new(),
                        }
                    } else {
                        Path::from(Name::new(&first))
                    };
                    let mut value = self.alloc_expr(Expr::Path(path), ptr.clone());
                    for field in parts {
                        value = self.alloc_expr(
                            Expr::Field {
                                expr: value,
                                name: Name::new(&field),
                            },
                            ptr.clone(),
                        );
                    }
                    (to_string(self, value), false)
                }
            };
            result = Some(match result {
                // The first piece makes the `String` the rest is appended to.
                None if is_text => to_string(self, operand),
                None => operand,
                Some(lhs) => self.alloc_expr(
                    Expr::BinaryOp {
                        lhs,
                        rhs: operand,
                        op: Some(BinaryOp::ArithOp(ArithOp::Add)),
                    },
                    ptr.clone(),
                ),
            });
        }
        result.unwrap_or_else(|| {
            let empty = self.alloc_expr(Expr::Literal(Literal::String(String::new())), ptr.clone());
            to_string(self, empty)
        })
    }
}
