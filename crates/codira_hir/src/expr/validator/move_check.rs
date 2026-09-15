//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 21, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! Opt-in per-function move checking via `@strict` (see
//! `spec/LANGUAGE_SPEC.md` section 17): a scoped v1 of "region-based
//! checking exactly where you opt in", not full lifetime inference or a
//! Rust-grade borrow checker. Only `consuming` parameters (section 14) are
//! tracked, and only within a function explicitly marked `@strict` -- every
//! other function is completely unaffected, matching this codebase's
//! existing convention of additive-only checks (`comptime_fold`,
//! `data_derive`, `refinement_check`, `heal_check` all share this shape:
//! narrow and real rather than broad and approximate).
//!
//! **The rule, precisely**: within an `@strict` function, a `consuming`
//! parameter's binding may be referenced (as a plain value -- an
//! `Expr::Path` resolving to it) at most once in the whole body. The first
//! reference is the move; a second reference is flagged as
//! `UseAfterConsume`.
//!
//! **Known, documented limitation (a false positive, not a false
//! negative)**: this is a flat count over every `Expr` in the body's arena
//! (`Body::exprs`), not a flow-sensitive walk. It does not know that
//! `if cond { consume(x) } else { consume(x) }` uses `x` on only one of
//! two mutually exclusive paths at runtime -- it will flag that as two
//! uses. A flow-sensitive version (branch-local cloned state, merged via
//! union across `if`/`match` arms -- the same shape
//! `uninitialized_access`'s already-working branch handling uses, mirrored
//! for "moved" instead of "initialized") is the natural next step, not
//! attempted this session. Until then, this check is reliable for
//! straight-line code (genuinely useful there -- it catches the most
//! common shape of "used something twice after handing off ownership") and
//! will over-flag legitimate branch-exclusive double use.
use std::collections::HashMap;

use super::ExprValidator;
use crate::{
    diagnostics::UseAfterConsume, resolve::resolver_for_expr, Expr, HasSource, Pat, PatId,
};

impl ExprValidator<'_> {
    pub(super) fn validate_move_checking(&self, sink: &mut crate::DiagnosticSink<'_>) {
        let fn_data = self.func.data(self.db);
        if !fn_data.is_strict() {
            return;
        }

        let consuming_pats: Vec<PatId> = self
            .body
            .params()
            .iter()
            .zip(fn_data.consuming_params())
            .filter(|(_, &is_consuming)| is_consuming)
            .map(|((pat, _), _)| *pat)
            .collect();
        if consuming_pats.is_empty() {
            return;
        }

        let mut use_counts: HashMap<PatId, u32> = HashMap::new();

        for (expr_id, expr) in self.body.exprs() {
            let Expr::Path(path) = expr else {
                continue;
            };
            let resolver = resolver_for_expr(self.db, self.body.owner(), expr_id);
            let Some((crate::resolve::ValueNs::LocalBinding(pat), _)) =
                resolver.resolve_path_as_value_fully(self.db, path)
            else {
                continue;
            };
            if !consuming_pats.contains(&pat) {
                continue;
            }

            let count = use_counts.entry(pat).or_insert(0);
            *count += 1;
            if *count > 1 {
                let Some(syntax) = self.body_source_map.expr_syntax(expr_id).map(|sp| {
                    sp.value
                        .either(|it| it.syntax_node_ptr(), |it| it.syntax_node_ptr())
                }) else {
                    continue;
                };
                let binding = match &self.body[pat] {
                    Pat::Bind { name } => name.clone(),
                    _ => continue,
                };
                sink.push(UseAfterConsume {
                    file: self.func.source(self.db).file_id,
                    use_site: syntax,
                    binding,
                });
            }
        }
    }
}
