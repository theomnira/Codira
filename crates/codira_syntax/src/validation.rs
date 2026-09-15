//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: August 6, 2026
//!
//! Functionality:
//! - Original module content restored; copyright header moved to top.
//!
//! This module contains the validation pass for the AST. See the [`validate`]
//! function for more information.

use crate::{
    ast,
    ast::{AstNode, VisibilityOwner},
    match_ast, SyntaxError, SyntaxNode,
};

/// A validation pass that checks that the AST is valid.
///
/// Even though the AST could be valid (aka without parse errors), it could
/// still be semantically incorrect. For example, a struct cannot be declared in
/// an extend block. This pass checks for these kinds of errors.
pub(crate) fn validate(root: &SyntaxNode) -> Vec<SyntaxError> {
    let mut errors = Vec::new();
    for node in root.descendants() {
        match_ast! {
            match node {
                ast::Extend(it) => validate_extend(it, &mut errors),
                _ => (),
            }
        }
    }

    errors
}

/// Validates the semantic validity of an `extend` block.
fn validate_extend(node: ast::Extend, errors: &mut Vec<SyntaxError>) {
    validate_extend_visibility(node.clone(), errors);
    validate_extend_items(node, errors);
}

/// Validate that the visibility of an extend block is undefined.
fn validate_extend_visibility(node: ast::Extend, errors: &mut Vec<SyntaxError>) {
    if let Some(vis) = node.visibility() {
        errors.push(SyntaxError::parse_error(
            "visibility is not allowed on extend blocks",
            vis.syntax.text_range(),
        ));
    }
}

/// Validate that only valid items are declared in an extend block. For
/// example, a struct cannot be declared in an extend block.
fn validate_extend_items(node: ast::Extend, errors: &mut Vec<SyntaxError>) {
    let Some(items) = node.extend_item_list() else {
        return;
    };

    for item in items.syntax.children() {
        match_ast! {
            match item {
                ast::FunctionDef(_it) => (),
                _ => errors.push(SyntaxError::parse_error("only functions are allowed in extend blocks", item.text_range())),
            }
        }
    }
}
