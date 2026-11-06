//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 5, 2026
//!
//! Functionality:
//! - Interpolated string literals: `f"x = {x}, p.y = {p.y:>8.3}"` is a
//!   `String`, lowered here to `"x = ".to_string() + x.to_string() + ", p.y
//!   = " + <p.y written as `>8.3` says>`.
//!
//! A hole names a local or a parameter, optionally followed by fields
//! (`{p.pos.x}`, `{self.name}`), as Rust's `format!("{x}")` names a variable:
//! no arbitrary expressions, so a hole never hides a call or a side effect.
//! A hole without a spec is converted with `to_string()`, so it type-checks
//! like that call would: a hole whose type has no `to_string` (an array, a
//! struct) is an error at the literal. A hole with a spec, `{x:spec}`,
//! becomes an [`Expr::Format`], which inference checks against the value's
//! type (see `builtin::format` for the grammar); `{x:?}` is the debug form,
//! which every type has. A width or a precision may name a variable,
//! `{x:>{w}.{p}}`. `{{` and `}}` are a literal brace. The pieces are
//! appended to one `String`, which ends at its buffer's fill after the
//! first append, so the appends after it extend that buffer in place
//! (doubling it when full) rather than copying the text so far.

use codira_syntax::{ast, AstPtr};

use super::{ArithOp, BinaryOp, Expr, ExprCollector, ExprId, Literal, LiteralError};
use crate::{
    builtin::dotted_name,
    name::Name,
    path::{Path, PathKind},
    FormatSpec,
};

/// One piece of an interpolated literal.
enum Piece {
    /// Text, still escaped as written.
    Text(String),
    /// The dotted name in a hole (a variable, then fields) and the format
    /// spec after its `:`, if it has one.
    Hole(Vec<String>, Option<String>),
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
                // The hole ends at the `}` that balances it: a spec may hold
                // a `{w}` of its own.
                let mut hole = String::new();
                let mut depth = 0usize;
                loop {
                    match chars.next()? {
                        '}' if depth == 0 => break,
                        c => {
                            match c {
                                '{' => depth += 1,
                                '}' => depth -= 1,
                                _ => {}
                            }
                            hole.push(c);
                        }
                    }
                }
                let (name, spec) = match hole.split_once(':') {
                    Some((name, spec)) => (name, Some(spec.to_owned())),
                    None => (hole.as_str(), None),
                };
                let parts = dotted_name(name)?;
                pieces.push(Piece::Text(std::mem::take(&mut text)));
                pieces.push(Piece::Hole(parts, spec.filter(|spec| !spec.is_empty())));
            }
            '}' => return None,
            c => text.push(c),
        }
    }
    pieces.push(Piece::Text(text));
    Some(pieces)
}

impl ExprCollector<'_> {
    /// The expression a hole's dotted name stands for: a variable (or
    /// `self`), then a field access per further part.
    fn collect_dotted_name(&mut self, parts: Vec<String>, ptr: &AstPtr<ast::Expr>) -> ExprId {
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
        value
    }

    /// Lowers the hole `{value:spec}` to an [`Expr::Format`], or reports a
    /// spec that does not parse (and lowers the hole to nothing).
    fn collect_formatted(
        &mut self,
        value: ExprId,
        spec: &str,
        ptr: &AstPtr<ast::Expr>,
    ) -> Option<ExprId> {
        match FormatSpec::parse(spec) {
            Ok(parsed) => {
                let width = parsed.width.map(|name| self.collect_dotted_name(name, ptr));
                let precision = parsed
                    .precision
                    .map(|name| self.collect_dotted_name(name, ptr));
                Some(self.alloc_expr(
                    Expr::Format {
                        value,
                        spec: parsed.spec,
                        width,
                        precision,
                    },
                    ptr.clone(),
                ))
            }
            Err(reason) => {
                self.diagnostics.push(super::ExprDiagnostic::LiteralError {
                    expr: value,
                    err: LiteralError::InvalidFormatSpec(format!("`{spec}`: {reason}")),
                });
                None
            }
        }
    }

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
            // Each piece is an operand of the appends, and whether it is a
            // `str` (which the first piece must be turned into a `String`
            // from) or already a `String`.
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
                Piece::Hole(parts, spec) => {
                    let value = self.collect_dotted_name(parts, &ptr);
                    match spec {
                        None => (to_string(self, value), false),
                        Some(spec) => match self.collect_formatted(value, &spec, &ptr) {
                            Some(formatted) => (formatted, false),
                            None => continue,
                        },
                    }
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
