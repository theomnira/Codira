//! Copyright (c) 2026 Omnira CJSC
//! Author: Tunjay Akbarli
//! Date: November 6, 2026
//!
//! Functionality:
//! - Regression tests for crashes the compiler fuzzer found
//!   (`crates/codira_compiler/tests/fuzz_compiler.rs`): each program, as the
//!   fuzzer reduced it, must be rejected with diagnostics instead of
//!   panicking the type checker.
use crate::utils::tests::diagnostics;

/// Checks that `source` is rejected with diagnostics (and does not panic).
fn rejected(source: &str) -> String {
    let diagnostics = diagnostics(source);
    assert!(
        !diagnostics.is_empty(),
        "`{source}` was accepted without a diagnostic"
    );
    diagnostics
}

/// The lexer reads `5e` as a float literal; parsing it as a number panicked.
#[test]
fn an_exponent_without_digits_is_an_invalid_literal() {
    rejected("func d(5e");
    let diagnostics = rejected("func d() -> f64 { 5e }");
    assert!(diagnostics.contains("invalid literal"), "{diagnostics}");
}
