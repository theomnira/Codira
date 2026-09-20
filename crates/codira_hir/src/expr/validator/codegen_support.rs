//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: September 17, 2026
//!
//! Reports constructs the code generator cannot lower yet, as diagnostics,
//! before it is asked to try.
//!
//! # Why this exists
//!
//! `codira_codegen` had `unimplemented!()` for several constructs that pass
//! the whole front end. Five of them were reachable from ordinary source:
//!
//! ```text
//! let s = "hello"        // panicked at ir/body.rs:567
//! let x: i64? = nil      // panicked at ir/body.rs:569
//! func id[T](x: T) -> T  // panicked at ir/ty.rs:240 / 438 / 452
//! ```
//!
//! The string case is no longer among them: a literal lowers to
//! `{ ptr, usize }` over constant bytes. What is left is `nil`, which still
//! has no representation to produce, and generic instantiation, which type
//! inference now handles but code generation does not.
//!
//! A `panic!` reaching a user is the worst outcome available: it aborts with
//! a Rust backtrace and an invitation to debug the compiler, gives no source
//! location, and -- for a toolchain meant for mission-critical work -- is
//! indistinguishable from a compiler bug, because it *is* one.
//!
//! `spec/EIDOS_RFC_002.md` §3.4 states the rule this module implements, in
//! the context of effects but as a general principle: what a backend gap
//! needs is "**a declining arm in codegen**: `Expr::Perform => None` plus a
//! `CannotCodegen` diagnostic -- *not* `unimplemented!()`".
//!
//! Declining here rather than inside codegen is what makes that practical.
//! `gen_expr` returns `Option`, where `None` already means "this expression
//! diverges"; overloading it with "the backend declined" would conflate two
//! very different things at every call site. Running as a validator instead
//! means the diagnostic carries a real source span, compilation stops before
//! codegen is entered, and codegen's own `unimplemented!()`s become what
//! they claim to be -- unreachable.
//!
//! # What is reported
//!
//! Only constructs that genuinely cannot be lowered today. This list is
//! meant to shrink: each entry is a backend gap, not a language rule, and
//! the diagnostics say so.

use codira_syntax::{AstNode, SyntaxNodePtr};

use super::ExprValidator;
use crate::{
    diagnostics::{DiagnosticSink, UnsupportedByCodegen},
    ty::Ty,
    Expr, HasSource, HirDisplay, Literal, TyKind,
};

impl ExprValidator<'_> {
    /// Pushes a diagnostic for each construct in this function that the
    /// code generator cannot lower.
    pub fn validate_codegen_support(&self, sink: &mut DiagnosticSink<'_>) {
        let file = self.func.source(self.db).file_id;

        // A generic declaration is reported once, against the function
        // itself, rather than once per mention of `T`. The gap is
        // instantiation, not any individual expression, and one diagnostic
        // naming the function is what the reader can act on.
        if let Some(ty) = self.first_type_parameter() {
            sink.push(UnsupportedByCodegen {
                file,
                node: SyntaxNodePtr::new(self.func.source(self.db).value.syntax()),
                what: format!(
                    "a generic function (`{}` is not instantiated)",
                    ty.display(self.db)
                ),
                because: "monomorphisation is not implemented yet, so there is no concrete type \
                          to generate code for"
                    .to_string(),
            });
            // One report is enough; the expression walk below would add
            // nothing but noise for the same root cause.
            return;
        }

        for (expr_id, expr) in self.body.exprs() {
            let Expr::Literal(literal) = expr else {
                continue;
            };
            let what = match literal {
                Literal::Nil => "`nil`",
                // A string literal lowers to `{ ptr, usize }` over constant
                // bytes now, so it is no longer a backend gap.
                Literal::String(_) | Literal::Bool(_) | Literal::Int(_) | Literal::Float(_) => {
                    continue
                }
            };

            let Some(node) = self.body_source_map.expr_syntax(expr_id).map(|ptr| {
                ptr.value
                    .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
            }) else {
                continue;
            };

            sink.push(UnsupportedByCodegen {
                file,
                node,
                what: what.to_string(),
                because: match literal {
                    Literal::String(_) => {
                        "there is no string representation in the runtime yet, so a string \
                         literal has nothing to lower to"
                    }
                    _ => {
                        "`Type?` optionals lower transparently to their base type, so there is \
                         no null representation for `nil` to produce"
                    }
                }
                .to_string(),
            });
        }
    }

    /// The first type parameter appearing in this function's signature,
    /// if any.
    ///
    /// The signature is enough: a type parameter can only enter a body
    /// through a parameter, the return type, or a generic type mentioned in
    /// them, so a function whose signature is concrete has no uninstantiated
    /// parameter to trip over. Checking the signature also means the
    /// diagnostic lands on the declaration, which is where the reader can
    /// do something about it.
    fn first_type_parameter(&self) -> Option<Ty> {
        self.func
            .self_param_ty(self.db)
            .into_iter()
            .chain(
                self.func
                    .params(self.db)
                    .into_iter()
                    .map(|p| p.ty().clone()),
            )
            .chain(std::iter::once(self.func.ret_type(self.db)))
            .find(contains_type_parameter)
    }
}

/// Whether `ty` is, or contains, a generic type parameter.
pub(crate) fn contains_type_parameter(ty: &Ty) -> bool {
    match ty.interned() {
        TyKind::TypeParam(..) => true,
        TyKind::Array(element) => contains_type_parameter(element),
        TyKind::Tuple(_, substs) => substs.iter().any(contains_type_parameter),
        // `TyKind::Struct` carries no substitution today (that is the same
        // gap this diagnostic is about), so there is nothing to look inside.
        _ => false,
    }
}
