//! Copyright (c) 2026 Omnira CJSC
//! Date: November 4, 2026
//!
//! Functionality:
//! - Rejects constructs the parser accepts but the compiler would not
//!   compile as written, so that none of them is silently accepted:
//!   - in-place parameters: the conventions `inout`, `mut` and `out`
//!     (`func bump(inout s: S)`) and the mutable reference types `mut T`
//!     and `&mut T` (`func bump(s: mut S)`). Every parameter is passed by
//!     copy, so a change the callee made through one would never reach the
//!     caller;
//!   - module-level bindings (`public let LIMIT: i64 = 10;`, and `var`).
//!     They are not lowered into anything a name can resolve to, so the
//!     binding was dropped and every use of it was an undefined name.
//!
//! These are found on the syntax tree, the only place they still exist
//! (lowering erases them: a parameter's convention and a reference type's
//! `mut` lower to the plain type, and a module-level binding to nothing a
//! module exposes). Every node of the file is visited, so a construct
//! cannot escape by being nested in an `extend` block, a trait, an
//! `extern` block or a function type.
//!
//! What stays accepted: `mut self`/`var self` (a mutable
//! *local copy*, which is what they mean), `consuming`, `borrowing` and
//! `read` (which a copy implements: the callee cannot change what the
//! caller sees through them), and `&T` (a read-only reference, which is
//! indistinguishable from a copy).

use codira_hir_input::FileId;
use codira_syntax::{
    ast::{self, AstNode, NameOwner},
    SyntaxKind, SyntaxNode, SyntaxNodePtr, T,
};

use crate::{diagnostics::ExprError, DiagnosticSink, HirDatabase};

/// Pushes a diagnostic for every in-place parameter and every module-level
/// binding of `file_id` into `sink`.
pub(crate) fn unsupported_syntax_diagnostics(
    db: &dyn HirDatabase,
    file_id: FileId,
    sink: &mut DiagnosticSink<'_>,
) {
    let source_file = db.parse(file_id).tree();
    for node in source_file.syntax().descendants() {
        if let Some(message) = message_for(&node) {
            sink.push(ExprError {
                file: file_id,
                node: SyntaxNodePtr::new(&node),
                message,
            });
        }
    }
}

/// The token of `kind` directly under `node`, if any.
fn has_token(node: &SyntaxNode, kind: SyntaxKind) -> bool {
    node.children_with_tokens()
        .any(|child| child.as_token().is_some_and(|t| t.kind() == kind))
}

/// Why `node` is rejected, or `None` when it is not one of the constructs
/// this module rejects.
fn message_for(node: &SyntaxNode) -> Option<String> {
    match node.kind() {
        SyntaxKind::PARAM => {
            let convention = [(T![inout], "inout"), (T![mut], "mut"), (T![out], "out")]
                .into_iter()
                .find(|(kind, _)| has_token(node, *kind))?
                .1;
            Some(format!(
                "`{convention}` parameters are not supported yet: every argument is passed \
                 by copy, so a change the function made to it would not reach the caller; \
                 return the new value instead (`func f(x: T) -> T`), or pass a class \
                 instance, whose fields the callee and the caller share"
            ))
        }
        SyntaxKind::REFERENCE_TYPE if has_token(node, T![mut]) => Some(
            "mutable references (`mut T`, `&mut T`) are not supported yet: every value is \
             passed by copy, so a change made through one would not reach its owner; return \
             the new value instead, or use a class, whose instances are shared"
                .to_owned(),
        ),
        SyntaxKind::CONST_DEF => {
            let def = ast::ConstDef::cast(node.clone())?;
            let name = def
                .name()
                .map_or_else(|| "_".to_owned(), |name| name.text().to_string());
            Some(if has_token(node, T![var]) || has_token(node, T![mut]) {
                format!(
                    "module-level `var` is not supported yet: there is no module-level state, \
                     so `{name}` would not exist at run time; keep the state in a value the \
                     functions that use it pass along, or in a class instance"
                )
            } else {
                format!(
                    "module-level `let` is not supported yet: `{name}` would not be defined, \
                     and no name can refer to it; write a function returning the value \
                     (`func {name}() -> T {{ .. }}`), as `std.math.constants` does"
                )
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::utils::tests::diagnostics;

    /// The message of every diagnostic of `source` that mentions `needle`.
    fn messages(source: &str, needle: &str) -> Vec<String> {
        diagnostics(source)
            .lines()
            .filter(|line| line.contains(needle))
            .map(|line| line.trim().to_owned())
            .collect()
    }

    #[test]
    fn in_place_parameter_conventions_are_rejected() {
        let source = r"
    struct S { v: i64 }
    func a(inout s: S) {}
    func b(mut s: S) {}
    func c(out s: S) {}
    func e(consuming s: S) {}
    func f(borrowing s: S) {}
    func g(read s: S) {}
    ";
        let found = messages(source, "parameters are not supported yet");
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found[0].contains("`inout` parameters"), "{found:#?}");
        assert!(found[1].contains("`mut` parameters"), "{found:#?}");
        assert!(found[2].contains("`out` parameters"), "{found:#?}");
        // `consuming`, `borrowing` and `read` are not reported.
        assert_eq!(diagnostics(source).lines().count(), 3);
    }

    #[test]
    fn mutable_reference_types_are_rejected_wherever_they_are_written() {
        let source = r#"
    struct S { v: i64 }
    func bump(s: mut S) { s.v = s.v + 1 }
    func bump2(s: &mut S) {}
    func read(s: &S) -> i64 { s.v }
    extern "C" { func ext(p: mut S); }
    func takes(f: func(mut S)) {}
    struct Holder { r: &mut S }
    "#;
        let found = messages(source, "mutable references");
        assert_eq!(found.len(), 5, "{found:#?}");
    }

    #[test]
    fn the_receiver_and_local_variables_stay_mutable_copies() {
        let source = r"
    struct S { v: i64 }
    extend S {
        func bumped(var self) -> S { self.v = self.v + 1; self }
        func bumped2(mut self) -> S { self.v = self.v + 1; self }
    }
    func f(x: i64) -> i64 { var y = x; y = y + 1; y }
    ";
        assert_eq!(diagnostics(source), "");
    }

    #[test]
    fn module_level_bindings_are_rejected() {
        let source = r"
    public let LIMIT: i64 = 10;
    let mut COUNT: i64 = 0;
    var STATE: i64 = 0;
    public func limit() -> i64 { 10 }
    ";
        let lets = messages(source, "module-level `let` is not supported yet");
        assert_eq!(lets.len(), 1, "{lets:#?}");
        assert!(lets[0].contains("`LIMIT`"), "{lets:#?}");
        let vars = messages(source, "module-level `var` is not supported yet");
        assert_eq!(vars.len(), 2, "{vars:#?}");
        assert!(vars[0].contains("`COUNT`"), "{vars:#?}");
        assert!(vars[1].contains("`STATE`"), "{vars:#?}");
    }
}
