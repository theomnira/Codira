//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Part of the Codira compiler and runtime toolchain.
use codira_syntax::TextRange;

use super::HirDiagnostic;
use crate::{Diagnostic, SourceAnnotation};

/// An error that is emitted when trying to access a field that is potentially
/// not yet initialized.
///
/// ```codira
/// # fn main() {
/// let a;
/// let b = a;    // `a` is possible not yet initialized
/// #}
/// ```
pub struct PossiblyUninitializedVariable<'db, 'diag, DB: codira_hir::HirDatabase> {
    _db: &'db DB,
    diag: &'diag codira_hir::diagnostics::PossiblyUninitializedVariable,
    value_name: String,
}

impl<DB: codira_hir::HirDatabase> Diagnostic for PossiblyUninitializedVariable<'_, '_, DB> {
    fn range(&self) -> TextRange {
        self.diag.highlight_range()
    }

    fn title(&self) -> String {
        format!("use of possibly-uninitialized `{}`", self.value_name)
    }

    fn primary_annotation(&self) -> Option<SourceAnnotation> {
        None
    }
}

impl<'db, 'diag, DB: codira_hir::HirDatabase> PossiblyUninitializedVariable<'db, 'diag, DB> {
    /// Constructs a new instance of `PossiblyUninitializedVariable`
    pub fn new(
        db: &'db DB,
        diag: &'diag codira_hir::diagnostics::PossiblyUninitializedVariable,
    ) -> Self {
        let parse = db.parse(diag.file);

        // Get the text of the value as a string
        let value_name = diag.pat.to_node(&parse.syntax_node()).text().to_string();

        PossiblyUninitializedVariable {
            _db: db,
            diag,
            value_name,
        }
    }
}
