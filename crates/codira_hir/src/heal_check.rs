//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 21, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
//!
//! SMT-checked healing-contract postconditions: `@heal(on: [...],
//! strategies: [...], postcondition: <expr>)` (see
//! `spec/HERACLES_Codira_Implementation.md` §2.2 and
//! `spec/self_healing_programming_language.md` §3.2 -- `codira_healing::
//! contract::HealingContract` is the runtime-side representation of the
//! same "(fault classes, strategies, postcondition) bound to a guarded
//! declaration" idea this module gives a compile-time-checked surface to).
//!
//! **One deliberate, documented deviation from the spec docs' own
//! pseudocode**: those examples write `postcondition: "result > 0 or
//! result == 0"` -- a *quoted string* containing a small DSL that isn't
//! Codira syntax at all (`or`, not `||`). Parsing a string's contents as a
//! second embedded language is a distinct, materially larger feature (a
//! sub-parser plus a surface-syntax design of its own) that risks exactly
//! the kind of syntax-mismatch bug a hasty implementation invites. This
//! module instead accepts `postcondition:` as a real, unquoted Codira
//! boolean expression -- `@heal(postcondition: result > 0 || result == 0)`
//! -- reusing `attribute_arg_list`'s existing `name: expr` parsing exactly
//! as every other named attribute argument already works (`@heal(on:
//! [...])` already parses `[...]` as a real `Expr`, not a string).
//!
//! **Scope, precisely**: this checks the postcondition is *satisfiable*
//! (via `crate::refinement_check::check_satisfiable`, binder `result`) --
//! not that the function body actually establishes it. Proving a function
//! body satisfies an arbitrary postcondition is full Hoare-logic
//! verification, far outside a single session's scope; catching a
//! postcondition that is *never* satisfiable by any return value at all
//! (e.g. `result > 0 && result < 0`) is the same well-formedness class of
//! bug `refinement_check` catches for refinement types, and is genuinely
//! useful on its own.
//!
//! `on:`/`strategies:` are intentionally not validated here: `on:` names
//! (`codira_healing::contract::FaultClass`) accept an arbitrary
//! `Custom(&str)` by design, so there is no "invalid fault class" to
//! reject, and `strategies:`' parameterized shape (`RetryWithBackoff(3)`,
//! `SubstituteAlternate(fallback)`) is real surface-syntax design work of
//! its own -- a real next step, not attempted this session.

use codira_syntax::ast::{self, AstNode, AttributeOwner};

use crate::refinement_check::{check_satisfiable, RefinementCheckResult};

/// If `func` carries a `@heal(...)` attribute with a `postcondition:`
/// argument, checks that argument for satisfiability. Returns `None` when
/// there is no `@heal` attribute, no `postcondition:` argument, or the
/// argument isn't a `Path`/`ArgList`-shaped attribute at all -- there is
/// nothing to check, not a `NotChecked` result (that's reserved for "we
/// found a postcondition but it uses a construct outside the checkable
/// subset").
pub(crate) fn check_heal_postcondition(func: &ast::FunctionDef) -> Option<RefinementCheckResult> {
    let heal_attr = func
        .attribute_list()?
        .attributes()
        .find(is_heal_attribute)?;
    let arg_list = heal_attr.arg_list()?;
    let postcondition = named_arg(&arg_list, "postcondition")?;
    Some(check_satisfiable("result", &postcondition))
}

fn is_heal_attribute(attr: &ast::Attribute) -> bool {
    let Some(segment) = attr.path().and_then(|p| p.segment()) else {
        return false;
    };
    matches!(
        segment.kind(),
        Some(ast::PathSegmentKind::Name(name_ref)) if name_ref.text() == "heal"
    )
}

/// Finds the value `Expr` for a `name: value` labeled argument inside an
/// attribute's argument list. `attribute_arg_list` (`codira_syntax`'s
/// parser) does not wrap `name:` in a dedicated node -- it's a loose
/// `IDENT COLON` token pair immediately before the value expression (see
/// that function's own doc comment) -- so this walks the raw
/// (trivia-filtered) child sequence looking for that exact three-element
/// pattern rather than using a structured accessor that doesn't exist.
fn named_arg(arg_list: &ast::ArgList, name: &str) -> Option<ast::Expr> {
    let elements: Vec<_> = arg_list
        .syntax()
        .children_with_tokens()
        .filter(|el| !el.kind().is_trivia())
        .collect();

    for i in 0..elements.len() {
        let Some(label) = elements[i].as_token() else {
            continue;
        };
        if label.kind() != codira_syntax::SyntaxKind::IDENT || label.text() != name {
            continue;
        }
        let Some(colon) = elements.get(i + 1).and_then(|el| el.as_token()) else {
            continue;
        };
        if colon.kind() != codira_syntax::T![:] {
            continue;
        }
        if let Some(value_node) = elements.get(i + 2).and_then(|el| el.as_node()) {
            if let Some(expr) = ast::Expr::cast(value_node.clone()) {
                return Some(expr);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use codira_syntax::{ast::ModuleItemOwner, SourceFile};

    use super::*;

    fn parse_fn(src: &str) -> ast::FunctionDef {
        let file = SourceFile::parse(src).tree();
        file.items()
            .find_map(|item| match item.kind() {
                ast::ModuleItemKind::FunctionDef(f) => Some(f),
                _ => None,
            })
            .expect("no function in fixture")
    }

    #[test]
    fn no_heal_attribute_is_none() {
        let f = parse_fn("func plain() -> i32 { 0 }");
        assert_eq!(check_heal_postcondition(&f), None);
    }

    #[test]
    fn heal_without_postcondition_is_none() {
        let f = parse_fn("@heal(on: [Timeout]) func risky() -> i32 { 0 }");
        assert_eq!(check_heal_postcondition(&f), None);
    }

    #[test]
    fn satisfiable_postcondition_is_ok() {
        let f =
            parse_fn("@heal(on: [Timeout], postcondition: result >= 0) func risky() -> i32 { 0 }");
        assert_eq!(
            check_heal_postcondition(&f),
            Some(RefinementCheckResult::Ok)
        );
    }

    #[test]
    fn impossible_postcondition_is_unsatisfiable() {
        let f =
            parse_fn("@heal(postcondition: result > 0 && result < 0) func risky() -> i32 { 0 }");
        assert_eq!(
            check_heal_postcondition(&f),
            Some(RefinementCheckResult::Unsatisfiable)
        );
    }

    #[test]
    fn or_form_is_understood() {
        let f =
            parse_fn("@heal(postcondition: result > 0 || result == 0) func risky() -> i32 { 0 }");
        assert_eq!(
            check_heal_postcondition(&f),
            Some(RefinementCheckResult::Ok)
        );
    }

    #[test]
    fn non_heal_attribute_is_ignored() {
        let f = parse_fn("@target(gpu) func risky() -> i32 { 0 }");
        assert_eq!(check_heal_postcondition(&f), None);
    }
}
