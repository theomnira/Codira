//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! Structural validation for `supervisor`/`child` blocks (see
//! `spec/self_healing_programming_language.md` section 3.5). These parse into
//! a full syntax tree (`codira_syntax`) but are not lowered into
//! name-resolvable HIR items -- there is no process-supervision runtime to
//! register them against (see `spec/LANGUAGE_SPEC.md` section 12's "explicitly
//! out of scope" note on the JIT/consensus machinery this paper otherwise
//! describes). This module operates directly on the raw syntax tree instead
//! of going through the usual item-diagnostic machinery, checking the one
//! thing that's genuinely well-scoped without that runtime: that a
//! supervisor tree's own declaration is well-formed (no child declared
//! twice, no config key set twice at the same nesting level).

use std::collections::HashSet;

use codira_hir_input::FileId;
use codira_syntax::{
    ast::{self, ModuleItemOwner, NameOwner},
    AstPtr,
};

use crate::{
    diagnostics::{DuplicateSupervisorChild, DuplicateSupervisorEntry},
    name::AsName,
    DiagnosticSink, HirDatabase, Name,
};

/// Pushes structural diagnostics for every `supervisor` block declared at
/// the top level of `file_id` into `sink`.
pub(crate) fn supervisor_diagnostics(
    db: &dyn HirDatabase,
    file_id: FileId,
    sink: &mut DiagnosticSink<'_>,
) {
    let source_file = db.parse(file_id).tree();
    for item in source_file.items() {
        if let ast::ModuleItemKind::SupervisorDef(supervisor) = item.kind() {
            validate_item_list(file_id, supervisor.supervisor_item_list(), sink);
        }
    }
}

/// Validates one `{ .. }` block's worth of `SupervisorEntry`/`ChildDef`
/// items (a `supervisor` or a `child`'s own body), then recurses into each
/// child's body in turn.
fn validate_item_list(
    file_id: FileId,
    list: Option<ast::SupervisorItemList>,
    sink: &mut DiagnosticSink<'_>,
) {
    let Some(list) = list else {
        return;
    };

    let mut seen_children: HashSet<Name> = HashSet::default();
    let mut seen_entries: HashSet<Name> = HashSet::default();

    for item in list.items() {
        match item.kind() {
            ast::SupervisorItemKind::ChildDef(child) => {
                if let Some(name_node) = child.name() {
                    let name = name_node.as_name();
                    if !seen_children.insert(name.clone()) {
                        sink.push(DuplicateSupervisorChild {
                            file: file_id,
                            duplicate: AstPtr::new(&child),
                            name,
                        });
                    }
                }
                validate_item_list(file_id, child.supervisor_item_list(), sink);
            }
            ast::SupervisorItemKind::SupervisorEntry(entry) => {
                if let Some(name_node) = entry.name() {
                    let name = name_node.as_name();
                    if !seen_entries.insert(name.clone()) {
                        sink.push(DuplicateSupervisorEntry {
                            file: file_id,
                            duplicate: AstPtr::new(&entry),
                            name,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use codira_hir_input::WithFixture;

    use crate::{mock::MockDatabase, DiagnosticSink, Module};

    fn diagnostics(content: &str) -> String {
        let (db, file_id) = MockDatabase::with_single_file(content);
        let module = Module::from_file(&db, file_id).unwrap();

        let mut messages = String::new();
        let mut sink = DiagnosticSink::new(|diag| {
            messages.push_str(&format!(
                "{:?}: {}\n",
                diag.highlight_range(),
                diag.message()
            ));
        });
        module.diagnostics(&db, &mut sink);
        drop(sink);
        messages.trim_end().to_string()
    }

    #[test]
    fn duplicate_child_name() {
        insta::assert_snapshot!(diagnostics(
            r#"
        supervisor DatabaseConnection {
            strategy: 1,

            child ConnPool {
                strategy: 2,
            }

            child ConnPool {
                strategy: 3,
            }
        }
        "#
        ));
    }

    #[test]
    fn duplicate_entry_key() {
        insta::assert_snapshot!(diagnostics(
            r#"
        supervisor DatabaseConnection {
            strategy: 1,
            strategy: 2,
        }
        "#
        ));
    }

    #[test]
    fn duplicate_entry_key_in_child() {
        insta::assert_snapshot!(diagnostics(
            r#"
        supervisor DatabaseConnection {
            strategy: 1,

            child ConnPool {
                strategy: 2,
                strategy: 3,
            }
        }
        "#
        ));
    }

    #[test]
    fn no_duplicates_is_clean() {
        insta::assert_snapshot!(diagnostics(
            r#"
        supervisor DatabaseConnection {
            strategy: 1,
            max_restarts: 5,

            child ConnPool {
                strategy: 2,
            }

            child CacheLayer {
                strategy: 2,
            }
        }
        "#
        ));
    }
}
