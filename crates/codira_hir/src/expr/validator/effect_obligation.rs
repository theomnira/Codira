//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 21, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! Effect-obligation checking (see `spec/LANGUAGE_SPEC.md` section 6):
//! "a function's `uses Effect, ...` clause is part of its type -- callers
//! must either be inside a matching `handle` block or themselves declare
//! `uses` and propagate the obligation". This module checks the *first*
//! half of that rule -- a function calling another function that declares
//! `uses Effect` must itself declare `uses Effect` (or a superset) -- for
//! ordinary calls (`Expr::Call`) and method calls (`Expr::MethodCall`).
//!
//! **Deliberately not checked here**: the *second* half ("or be inside a
//! matching `handle` block"). `perform`/`handle` expressions still lower to
//! `Expr::Missing` (see `expr.rs`'s big match and its own doc comment) --
//! there is no HIR representation yet of which effect a `handle` block
//! discharges, so this pass cannot know a call is covered by one. Concrete
//! consequence: this pass will flag a call inside a correct `handle Effect
//! { .. }` block as an undeclared-effect error today, a real (documented,
//! not hidden) false positive until `handle` gets real HIR lowering. Until
//! then, this check is only reliable for code with no `handle` blocks --
//! genuinely useful there (it's real, tested detection of a real class of
//! bug), not a complete implementation of the spec's rule.
use super::ExprValidator;
use crate::{diagnostics::UndeclaredEffect, resolve::resolver_for_expr, Expr, HasSource};

impl ExprValidator<'_> {
    /// Checks every call in this function's body against its own declared
    /// `uses` clause.
    pub(super) fn validate_effect_obligations(&self, sink: &mut crate::DiagnosticSink<'_>) {
        if !self.has_any_call() {
            return;
        }
        let caller_data = self.func.data(self.db);
        let caller_effects = caller_data.effects();

        for (expr_id, expr) in self.body.exprs() {
            let callee_fn = match expr {
                Expr::Call { callee, .. } => {
                    let resolver = resolver_for_expr(self.db, self.body.owner(), *callee);
                    match &self.body[*callee] {
                        Expr::Path(path) => resolver
                            .resolve_path_as_value_fully(self.db, path)
                            .and_then(|(ns, _)| match ns {
                                crate::resolve::ValueNs::FunctionId(id) => Some(id),
                                _ => None,
                            }),
                        _ => None,
                    }
                }
                Expr::MethodCall { .. } => self.infer.method_resolutions.get(&expr_id).copied(),
                _ => None,
            };

            let Some(callee_fn) = callee_fn else {
                continue;
            };
            let callee: crate::Function = callee_fn.into();
            let callee_data = callee.data(self.db);

            for effect in callee_data.effects() {
                if caller_effects.contains(effect) {
                    continue;
                }
                let Some(syntax) = self.body_source_map.expr_syntax(expr_id).map(|sp| {
                    sp.value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                }) else {
                    continue;
                };
                sink.push(UndeclaredEffect {
                    file: self.func.source(self.db).file_id,
                    call: syntax,
                    effect: effect.clone(),
                    callee: callee_data.name().clone(),
                });
            }
        }
    }

    fn has_any_call(&self) -> bool {
        self.body
            .exprs()
            .any(|(_, e)| matches!(e, Expr::Call { .. } | Expr::MethodCall { .. }))
    }
}
