//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! This module provides conversion from a `codira_hir::Diagnostics` to a
//! `crate::Diagnostics`.

mod access_unknown_field;
mod duplicate_definition_error;
mod expected_function;
mod exported_private;
mod mismatched_type;
mod missing_fields;
mod possibly_unitialized_variable;
mod unresolved_type;
mod unresolved_value;

use codira_hir::Diagnostic as HirDiagnostic;
use codira_syntax::TextRange;

use crate::{Diagnostic, DiagnosticForWith, SourceAnnotation};

// Provides conversion of a codira_hir::Diagnostic to a crate::Diagnostic. This
// requires a database for most operations.
impl<DB: codira_hir::HirDatabase> DiagnosticForWith<DB> for dyn codira_hir::Diagnostic {
    fn with_diagnostic<R, F: FnMut(&dyn Diagnostic) -> R>(&self, with: &DB, mut f: F) -> R {
        if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::UnresolvedValue>() {
            f(&unresolved_value::UnresolvedValue::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::UnresolvedType>() {
            f(&unresolved_type::UnresolvedType::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::ExpectedFunction>() {
            f(&expected_function::ExpectedFunction::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::MismatchedType>() {
            f(&mismatched_type::MismatchedType::new(with, v))
        } else if let Some(v) =
            self.downcast_ref::<codira_hir::diagnostics::PossiblyUninitializedVariable>()
        {
            f(&possibly_unitialized_variable::PossiblyUninitializedVariable::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::AccessUnknownField>() {
            f(&access_unknown_field::AccessUnknownField::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::DuplicateDefinition>()
        {
            f(&duplicate_definition_error::DuplicateDefinition::new(
                with, v,
            ))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::MissingFields>() {
            f(&missing_fields::MissingFields::new(with, v))
        } else if let Some(v) = self.downcast_ref::<codira_hir::diagnostics::ExportedPrivate>() {
            f(&exported_private::ExportedPrivate::new(with, v))
        } else {
            f(&GenericHirDiagnostic { diagnostic: self })
        }
    }
}

/// Diagnostic handler for HIR diagnostics that do not have a specialized
/// implementation.
struct GenericHirDiagnostic<'diag> {
    diagnostic: &'diag dyn codira_hir::Diagnostic,
}

impl Diagnostic for GenericHirDiagnostic<'_> {
    fn range(&self) -> TextRange {
        self.diagnostic.highlight_range()
    }

    fn title(&self) -> String {
        self.diagnostic.message()
    }

    fn primary_annotation(&self) -> Option<SourceAnnotation> {
        None
    }
}
